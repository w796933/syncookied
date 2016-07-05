#![feature(test)]
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate log;
extern crate syslog;
extern crate libc;
extern crate pnet;
extern crate crossbeam;
extern crate scheduler;
extern crate clap;
extern crate yaml_rust;
extern crate parking_lot;
extern crate intmap;
extern crate fnv;
extern crate bounded_spsc_queue as spsc;
extern crate chan_signal;
extern crate pcap;
extern crate bpfjit;
extern crate chrono;
extern crate influent;

use std::fmt;
use std::cell::RefCell;
use std::thread;
use std::path::PathBuf;
use std::time::Duration;
use std::sync::atomic::{AtomicUsize};
use std::sync::Arc;
use std::net::Ipv4Addr;
use std::str::FromStr;
use pnet::util::MacAddr;
use parking_lot::{RwLock,Mutex,Condvar};
use intmap::LocklessIntMap;

use std::collections::BTreeMap;
use std::hash::BuildHasherDefault;

use clap::{Arg, App, AppSettings, SubCommand};
use chan_signal::Signal;

use bpfjit::BpfJitFilter;

mod netmap;
mod cookie;
mod sha1;
mod packet;
mod csum;
mod util;
mod tx;
mod rx;
mod uptime;
mod config;
mod filter;
mod logging;
mod metrics;
use uptime::UptimeReader;
use packet::{IngressPacket};
use netmap::{Direction,NetmapDescriptor};

// TODO: split "routing" into its own file
lazy_static! {
    /* maps public IP to tcp parameters */
    static ref GLOBAL_HOST_CONFIGURATION: RwLock<BTreeMap<Ipv4Addr, HostConfiguration>> = {
        let hm = BTreeMap::new();
        RwLock::new(hm)
    };
}

// per thread "routing" table
// it's updated periodically in Rx/Tx threads
// lets us avoid contention
thread_local!(pub static LOCAL_ROUTING_TABLE: RefCell<BTreeMap<Ipv4Addr, HostConfiguration>> = {
    let hm = BTreeMap::new();
    RefCell::new(hm)
});

#[derive(Clone)]
struct RecentSentTable {
    map: LocklessIntMap<BuildHasherDefault<fnv::FnvHasher>>,
}

impl fmt::Debug for RecentSentTable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "RecentSentTable")
    }
}

impl RecentSentTable {
    fn new() -> Self {
        RecentSentTable {
            map: LocklessIntMap::new(65536 /* number of ports */, BuildHasherDefault::<fnv::FnvHasher>::default())
        }
    }

    pub fn touch(&mut self, dest_port: u16, timestamp: usize) {
        let key: usize = dest_port as usize;
        self.map.insert(key, timestamp);
    }

    pub fn get_last_touched(&self, dest_port: u16) -> Option<usize> {
        let key: usize = dest_port as usize;
        self.map.get(key)
    }
}

#[derive(Clone)]
struct StateTable {
    map: LocklessIntMap<BuildHasherDefault<fnv::FnvHasher>>,
}

impl fmt::Debug for StateTable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let entries = self.map.entries();
        fn decode_key(k: usize) -> (Ipv4Addr, u16, u16) {
            let ip = Ipv4Addr::from((k >> 32) as u32);
            let src_port = ((k & 0xffffffff) >> 16) as u16;
            let dst_port = k as u16;
            (ip, src_port, dst_port)
        }
        fn decode_val(v: usize) -> ConnState {
            ConnState::from(v - 1)
        }
        for entry in entries.iter() {
            if entry.key() == 0 || entry.value() == 0 {
                continue;
            }
            write!(f, "{:?} -> {:?}", decode_key(entry.key()), decode_val(entry.value()));
        }
        write!(f, "")
    }
}

#[derive(Debug)]
enum ConnState {
    Established, // first ACK received and valid
    Closing, // FIN received
}

impl From<usize> for ConnState {
    fn from(x: usize) -> Self {
        match x {
            0 => ConnState::Established,
            1 => ConnState::Closing,
            x => panic!("invalid connection state {}", x),
        }
    }
}

// TODO: implement removal in LocklessIntMap
// note: currently unused
impl StateTable {
    fn new(size: usize) -> Self {
        StateTable {
            map: LocklessIntMap::new(size, BuildHasherDefault::<fnv::FnvHasher>::default())
        }
    }

    pub fn set_state(&mut self, ip: Ipv4Addr, source_port: u16, dest_port: u16, state: ConnState) {
        let int_ip = u32::from(ip) as usize;
        let key: usize = int_ip << 32
                         | (source_port as usize) << 16
                         | dest_port as usize;
        self.map.insert(key, (state as usize) + 1);
    }
    
    pub fn get_state(&self, ip: Ipv4Addr, source_port: u16, dest_port: u16) -> Option<ConnState> {
        let int_ip = u32::from(ip) as usize;
        let key: usize = int_ip << 32
                         | (source_port as usize) << 16
                         | dest_port as usize;
        self.map.get(key).map(|val| ConnState::from(val - 1))
    }

    pub fn delete_state(&mut self, ip: Ipv4Addr, source_port: u16, dest_port: u16) {
        let int_ip = u32::from(ip) as usize;
        let key: usize = int_ip << 32
                         | (source_port as usize) << 16
                         | dest_port as usize;
        self.map.delete(key);
    }
}

// TODO: rename to sth. more appropriate (FibTable, ConfigTable?)
pub struct RoutingTable;

impl RoutingTable {
    fn add_host(ip: Ipv4Addr, mac: MacAddr, default_policy: filter::FilterAction, filters: Vec<(BpfJitFilter,filter::FilterAction)>) {
        info!("Configuration: {} -> {} Filters: {} Default policy: {:?}", ip, mac, filters.len(), default_policy);
        let host_conf = HostConfiguration::new(mac, filters, default_policy);
        let mut w = GLOBAL_HOST_CONFIGURATION.write();

        w.insert(ip, host_conf);
    }

    fn clear() {
        let mut w = GLOBAL_HOST_CONFIGURATION.write();
        w.clear();
    }

    pub fn get_ips() -> Vec<Ipv4Addr> {
        let r = GLOBAL_HOST_CONFIGURATION.read();
        r.keys().cloned().collect()
    }

    pub fn sync_tables() {
        LOCAL_ROUTING_TABLE.with(|rt| {
            let ips = ::RoutingTable::get_ips();
            let mut cache = rt.borrow_mut();
            cache.clear();
            for ip in ips.iter() {
                ::RoutingTable::with_host_config_global(*ip, |hc| { cache.insert(*ip, hc.to_owned()); });
            }
        })
    }

    pub fn dump_states() {
        let ips = Self::get_ips();
        LOCAL_ROUTING_TABLE.with(|rt| {
            for ip in ips {
                let r = rt.borrow();
                if let Some(ref hc) = r.get(&ip) {
                    println!("[{}] {:?}", ip, hc.state_table);
                }
            }
        });
    }

    pub fn with_host_config<F>(ip: Ipv4Addr, mut f: F) -> Option<()> where F: FnMut(&HostConfiguration) {
        LOCAL_ROUTING_TABLE.with(|rt| {
            let r = rt.borrow();
            if let Some(hc) = r.get(&ip) {
                f(hc);
                Some(())
            } else {
                None
            }
        })
    }

    pub fn with_host_config_mut<F>(ip: Ipv4Addr, mut f: F) -> Option<()> where F: FnMut(&mut HostConfiguration) {
        LOCAL_ROUTING_TABLE.with(|rt| {
            let mut cache = rt.borrow_mut();
            if let Some(hc) = cache.get_mut(&ip) {
                f(hc);
                Some(())
            } else {
                None
            }
        })
    }

    pub fn with_host_config_global<F>(ip: Ipv4Addr, mut f: F) -> Option<()> where F: FnMut(&HostConfiguration) {
        let r = GLOBAL_HOST_CONFIGURATION.read();
        if let Some(hc) = r.get(&ip) {
            f(hc);
            Some(())
        } else {
            None
        }
    }

    pub fn with_host_config_global_mut<F>(ip: Ipv4Addr, mut f: F) -> Option<()> where F: FnMut(&mut HostConfiguration) {
        let mut w = GLOBAL_HOST_CONFIGURATION.write();
        if let Some(hc) = w.get_mut(&ip) {
            f(hc);
            Some(())
        } else {
            None
        }
    }
}

pub struct HostConfiguration {
    mac: MacAddr,
    tcp_timestamp: u32,
    tcp_cookie_time: u32,
    hz: u32,
    syncookie_secret: [[u32;17];2],
    state_table: StateTable,
    recent_table: RecentSentTable,
    filters: Arc<Vec<(BpfJitFilter,filter::FilterAction)>>,
    default: filter::FilterAction,
}

impl HostConfiguration {
    fn new(mac: MacAddr, filters: Vec<(BpfJitFilter,filter::FilterAction)>, default: filter::FilterAction) -> Self {
        HostConfiguration {
            mac: mac,
            tcp_timestamp: 0,
            tcp_cookie_time: 0,
            hz: 300,
            syncookie_secret: [[0;17];2],
            state_table: StateTable::new(1024 * 1024),
            recent_table: RecentSentTable::new(),
            filters: Arc::new(filters),
            default: default,
        }
    }
}

impl Clone for HostConfiguration {
    fn clone(&self) -> Self {
        HostConfiguration {
            mac: self.mac,
            tcp_timestamp: self.tcp_timestamp,
            tcp_cookie_time: self.tcp_cookie_time,
            hz: self.hz,
            syncookie_secret: self.syncookie_secret.clone(),
            state_table: self.state_table.clone(),
            recent_table: self.recent_table.clone(),
            filters: self.filters.clone(),
            default: self.default,
        }
    }
}

#[derive(Debug)]
pub struct ForwardedPacket {
    pub slot_ptr: usize,
    pub buf_ptr: usize,
    pub destination_mac: MacAddr,
}

pub enum OutgoingPacket {
    Ingress(IngressPacket),
    Forwarded((usize, usize, MacAddr)),
}

// spawn threads updating tcp cookie secrets / uptime
fn run_uptime_readers(reload_lock: Arc<(Mutex<bool>, Condvar)>, uptime_readers: Vec<(Ipv4Addr, Box<UptimeReader>)>) {
    let one_sec = Duration::new(1, 0);
    crossbeam::scope(|scope| {
        for (ip, uptime_reader) in uptime_readers.into_iter() {
            let reload_lock = reload_lock.clone();
            info!("Uptime reader for {} starting", ip);
            scope.spawn(move|| loop {
                ::util::set_thread_name(&format!("syncookied/{}", ip));
                match uptime_reader.read() {
                    Ok(buf) => uptime::update(ip, buf),
                    Err(err) => error!("Failed to read uptime: {:?}", err),
                }
                thread::sleep(one_sec);
                let &(ref lock, _) = &*reload_lock;
                let time_to_die = lock.lock();
                if *time_to_die {
                    info!("Uptime reader for {} exiting", ip);
                    break;
                }
            });
        }
    });
    let &(ref lock, ref cv) = &*reload_lock;
    let mut time_to_die = lock.lock();
    *time_to_die = false;
    cv.notify_all();
    info!("All uptime readers dead");
}

fn handle_signals(path: PathBuf, reload_lock: Arc<(Mutex<bool>, Condvar)>) {
    let signal = chan_signal::notify(&[Signal::HUP, Signal::INT, Signal::USR1]);
    thread::spawn(move || loop {
        ::util::set_thread_name("syncookied/sig");
        match signal.recv().unwrap() {
            Signal::HUP => {
                info!("SIGHUP received, reloading configuration");
                match config::configure(&path) {
                    Ok(data) => {
                        let uptime_readers = data.iter().map(|&(ip, ref addr)|
                                                (ip, Box::new(uptime::UdpReader::new(addr.to_owned())) as Box<UptimeReader>)
                                               ).collect();
                        /* wait for old readers to die */
                        {
                            let &(ref lock, ref cv) = &*reload_lock;
                            let mut time_to_die = lock.lock();
                            *time_to_die = true;
                            while *time_to_die {
                                cv.wait(&mut time_to_die); // unlocks mutex
                            }
                        }
                        info!("Old readers are dead, all hail to new readers");
                        let reload_lock = reload_lock.clone();
                        thread::spawn(move || run_uptime_readers(reload_lock.clone(), uptime_readers));
                    },
                    Err(e) => error!("Error parsing config file {}: {:?}", path.display(), e),
                }
            },
            Signal::USR1 => {
                ::RoutingTable::sync_tables();
                ::RoutingTable::dump_states();
            },
            Signal::INT => {
                use std::process;
                info!("SIGINT received, exiting");
                process::exit(0);
            },
            _ => {
                error!("Unhandled signal {:?}, ignoring", signal);
            }
        }
    });
}

// TODO: too many parameters, put them into a struct
fn run(config: PathBuf, rx_iface: &str, tx_iface: &str,
       rx_mac: MacAddr, tx_mac: MacAddr,
       qlen: u32, first_cpu: usize,
       uptime_readers: Vec<(Ipv4Addr, Box<UptimeReader>)>,
       metrics_server: Option<&str>) {
    let rx_nm = Arc::new(Mutex::new(NetmapDescriptor::new(rx_iface).unwrap()));
    let multi_if = rx_iface != tx_iface;
    let tx_nm = if multi_if {
         let rx_nm = &*rx_nm.lock();
         Arc::new(Mutex::new(NetmapDescriptor::new_with_memory(tx_iface, rx_nm).unwrap()))
     } else {
         rx_nm.clone()
    };
    let rx_count = {
        let rx_nm = rx_nm.lock();
        rx_nm.get_rx_rings_count()
    };
    let tx_count = {
        let tx_nm = tx_nm.lock();
        tx_nm.get_tx_rings_count()
    };
    info!("{} Rx rings @ {}, {} Tx rings @ {} Queue: {}", rx_count, rx_iface, tx_count, tx_iface, qlen);
    if tx_count < rx_count {
        panic!("We need at least as much Tx rings as Rx rings")
    }

    crossbeam::scope(|scope| {
        let reload_lock = Arc::new((Mutex::new(false), Condvar::new()));
        handle_signals(config, reload_lock.clone());

        scope.spawn(move || 
                    run_uptime_readers(reload_lock.clone(), uptime_readers));

        // we spawn a thread per RX/TX queue
        for ring in 0..rx_count {
            let ring = ring;
            let (tx, rx) = spsc::make(qlen as usize);
            let (f_tx, f_rx) = spsc::make(qlen as usize);
            let pair = Arc::new(AtomicUsize::new(0));
            let rx_pair = pair.clone();

            {
                let rx_nm = rx_nm.clone();

                scope.spawn(move || {
                    info!("Starting RX thread for ring {} at {}", ring, rx_iface);
                    let mut ring_nm = {
                        let nm = rx_nm.lock();
                        nm.clone_ring(ring, Direction::Input).unwrap()
                    };
                    let cpu = first_cpu + ring as usize;
                    rx::Receiver::new(ring, cpu, f_tx, tx, &mut ring_nm, rx_pair, rx_mac.clone(), metrics_server).run();
                });
            }

            /* Start an ARP thread to keep switch from forgetting about us */
            /*
            if multi_if && ring == 0 {
                    let rx_nm = rx_nm.clone();

                    scope.spawn(move || {
                    info!("Starting ARP thread for ring {} at {}", ring, rx_iface);
                    let mut ring_nm = {
                        let nm = rx_nm.lock().unwrap();
                        nm.clone_ring(ring, Direction::Output).unwrap()
                    };
                    let cpu = ring as usize;
                    /* XXX: replace hardcoded IPs */
                    arp::Sender::new(ring, cpu, &mut ring_nm, rx_mac.clone(), Ipv4Addr::new(185,50,25,2), Ipv4Addr::new(185,50,25,1)).run();
                });
            }
            */

            /* second half */
            let f_rx = if multi_if {
                let f_tx_nm = rx_nm.clone();
                let pair = pair.clone();
                scope.spawn(move || {
                    info!("Starting TX thread for ring {} at {}", ring, rx_iface);
                    let mut ring_nm = {
                        let nm = f_tx_nm.lock();
                        nm.clone_ring(ring, Direction::Output).unwrap()
                    };
                    let cpu = first_cpu + ring as usize; /* we assume queues/rings are bound to cpus */
                    tx::Sender::new(ring, cpu, None, Some(f_rx), &mut ring_nm, pair, rx_mac.clone(), metrics_server).run();
                });
                None
            } else {
                Some(f_rx)
            };

            let tx_nm = tx_nm.clone();
            scope.spawn(move || {
                info!("Starting TX thread for ring {} at {}", ring, tx_iface);
                let mut ring_nm = {
                    let nm = tx_nm.lock();
                    nm.clone_ring(ring, Direction::Output).unwrap()
                };
                /* We assume that in multi_if configuration 
                 *  - RX queues are bound to [first_cpu .. first_cpu + rx_count]
                 *  - TX queues are bound to [ first_cpu + rx_count .. first_cpu + rx_count + tx_count ]
                 */
                let cpu = if multi_if {
                    rx_count as usize
                } else {
                    0
                } + first_cpu + ring as usize;
                tx::Sender::new(ring, cpu, Some(rx), f_rx, &mut ring_nm, pair, tx_mac, metrics_server).run();
            });
        }

        /*
        {
            let nm = rx_nm.clone();
            let ring = rx_count;

            scope.spawn(move || {
                    info!("Starting Host RX thread for ring {}", ring);
                    let mut ring_nm = {
                        let nm = nm.lock().unwrap();
                        nm.clone_ring(ring, Direction::Input).unwrap()
                    };
                    host_rx_loop(ring as usize, &mut ring_nm)
            });

        }
        */
    });
}

fn main() {
    let matches = App::new("syncookied")
                              .version("0.1.9")
                              .author("Alexander Polyakov <apolyakov@beget.ru>")
                              .setting(AppSettings::SubcommandsNegateReqs)
                              .subcommand(
                                SubCommand::with_name("server")
                                .about("Run /proc/beget_uptime reader")
                                .arg(Arg::with_name("addr")
                                     .takes_value(true)
                                     .value_name("ip:port")
                                     .help("ip:port to listen on"))
                              )
                              .arg(Arg::with_name("config")
                                   .short("c")
                                   .long("config")
                                   .value_name("file")
                                   .help("path to hosts.yml file")
                                   .required(false)
                                   .takes_value(true))
                              .arg(Arg::with_name("in")
                                   .short("i")
                                   .long("input-interface")
                                   .value_name("iface")
                                   .help("Interface to receive packets on")
                                   .required(true)
                                   .takes_value(true))
                               .arg(Arg::with_name("out")
                                   .short("o")
                                   .long("output-interface")
                                   .value_name("iface")
                                   .help("Interface to send packets on (input interface will be used if not set)")
                                   .takes_value(true))
                               .arg(Arg::with_name("in-mac")
                                    .short("I")
                                    .required(false)
                                    .long("input-mac")
                                    .value_name("xx:xx:xx:xx:xx:xx")
                                    .help("Input interface mac address")
                                    .takes_value(true))
                               .arg(Arg::with_name("out-mac")
                                    .short("O")
                                    .required(false)
                                    .long("output-mac")
                                    .value_name("xx:xx:xx:xx:xx:xx")
                                    .help("Output interface mac address")
                                    .takes_value(true))
                               .arg(Arg::with_name("qlen")
                                    .short("N")
                                    .required(false)
                                    .long("queue-length")
                                    .help("Length of buffer queue")
                                    .takes_value(true))
                               .arg(Arg::with_name("cpu")
                                    .short("C")
                                    .required(false)
                                    .long("first-cpu")
                                    .help("First cpu to use for Rx")
                                    .takes_value(true))
                               .arg(Arg::with_name("debug")
                                    .long("debug")
                                    .help("Log to stdout")
                                    .takes_value(false))
                               .arg(Arg::with_name("metrics-server")
                                    .long("metrics-server")
                                    .required(false)
                                    .help("host:port of influxdb udp listener")
                                    .takes_value(true))
                               .get_matches();

    if let Some(matches) = matches.subcommand_matches("server") {
        let addr = matches.value_of("addr").unwrap_or("127.0.0.1:1488"); 
        uptime::run_server(addr);
    } else {
        println!("Hostname: {}", util::get_host_name().unwrap());
        let conf = matches.value_of("config").unwrap_or("hosts.yml");
        let rx_iface = matches.value_of("in").expect("Expected valid input interface");
        let tx_iface = matches.value_of("out").unwrap_or(rx_iface);
        let rx_mac: MacAddr = matches.value_of("in-mac")
                                .map(str::to_owned)
                                .or_else(|| util::get_iface_mac(rx_iface).ok())
                                .map(|mac| util::parse_mac(&mac).expect("Expected valid mac")).unwrap();
        let tx_mac: MacAddr = matches.value_of("out-mac")
                                .map(str::to_owned)
                                .or_else(|| util::get_iface_mac(tx_iface).ok())
                                .map(|mac| util::parse_mac(&mac).expect("Expected valid mac")).unwrap_or(rx_mac.clone());
        let ncpus = util::get_cpu_count();
        let qlen = matches.value_of("qlen")
                          .map(|x| u32::from_str(x).expect("Expected number for queue length"))
                          .unwrap_or(1024 * 1024);
        let cpu = matches.value_of("cpu")
                         .map(|x| usize::from_str(x).expect("Expected cpu number"))
                         .unwrap_or(0);
        let metrics_server = matches.value_of("metrics-server");

        let config_path = PathBuf::from(conf);
        let debug = matches.is_present("debug");
        logging::initialize(debug);
        match config::configure(&config_path) {
            Ok(config) => {
                debug!("Config file {} loaded", config_path.display());
                let uptime_readers =
                    config.iter().map(|&(ip, ref addr)|
                        (ip, Box::new(uptime::UdpReader::new(addr.to_owned())) as Box<UptimeReader>)
                    ).collect();
                info!("interfaces: [Rx: {}/{}, Tx: {}/{}] Cores: {}", rx_iface, rx_mac, tx_iface, tx_mac, ncpus);
                run(config_path, &rx_iface, &tx_iface, rx_mac, tx_mac, qlen, cpu, uptime_readers, metrics_server);
            },
            Err(e) => error!("Error parsing config file {}: {:?}", config_path.display(), e),
        }
    }
}
