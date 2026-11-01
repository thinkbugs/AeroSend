use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

const AIRPLAY_SERVICE: &str = "_airplay._tcp.local.";
const RAOP_SERVICE: &str = "_raop._tcp.local.";

#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub device_type: String,
    pub properties: HashMap<String, String>,
}

pub struct DiscoveryService {
    daemon: Arc<ServiceDaemon>,
    devices: Arc<Mutex<HashMap<String, DiscoveredDevice>>>,
    tx: Option<mpsc::Sender<DiscoveryEvent>>,
}

#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    DeviceFound(DiscoveredDevice),
    DeviceLost(String),
}

impl DiscoveryService {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let daemon = ServiceDaemon::new()?;
        Ok(Self {
            daemon: Arc::new(daemon),
            devices: Arc::new(Mutex::new(HashMap::new())),
            tx: None,
        })
    }

    pub fn start(&mut self) -> mpsc::Receiver<DiscoveryEvent> {
        let (tx, rx) = mpsc::channel(32);
        self.tx = Some(tx.clone());
        self.browse_service(AIRPLAY_SERVICE, "airplay", tx.clone());
        self.browse_service(RAOP_SERVICE, "raop", tx);
        info!("mDNS discovery started");
        rx
    }

    fn browse_service(
        &self,
        service_type: &str,
        device_type: &str,
        tx: mpsc::Sender<DiscoveryEvent>,
    ) {
        let receiver = match self.daemon.browse(service_type) {
            Ok(r) => r,
            Err(e) => {
                error!("Failed to browse {}: {}", service_type, e);
                return;
            }
        };

        let devices = Arc::clone(&self.devices);
        let dtype = device_type.to_string();

        std::thread::spawn(move || {
            while let Ok(event) = receiver.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let device = service_info_to_device(&info, &dtype);
                        let id = device.id.clone();
                        debug!("Device resolved: {} at {}:{}", device.name, device.host, device.port);

                        if let Ok(mut devs) = devices.lock() {
                            devs.insert(id, device.clone());
                        }
                        let _ = tx.blocking_send(DiscoveryEvent::DeviceFound(device));
                    }
                    ServiceEvent::ServiceRemoved(_service_type, full_name) => {
                        debug!("Device removed: {}", full_name);
                        if let Ok(mut devs) = devices.lock() {
                            devs.remove(&full_name);
                        }
                        let _ = tx.blocking_send(DiscoveryEvent::DeviceLost(full_name));
                    }
                    ServiceEvent::SearchStarted(st) => {
                        debug!("Search started for {}", st);
                    }
                    ServiceEvent::SearchStopped(st) => {
                        debug!("Search stopped for {}", st);
                    }
                    _ => {}
                }
            }
        });
    }

    pub fn stop(&self) {
        if let Err(e) = self.daemon.shutdown() {
            warn!("Failed to stop mDNS daemon: {}", e);
        }
        info!("mDNS discovery stopped");
    }

    pub fn get_devices(&self) -> Vec<DiscoveredDevice> {
        self.devices
            .lock()
            .map(|d| d.values().cloned().collect())
            .unwrap_or_default()
    }
}

fn service_info_to_device(info: &ServiceInfo, device_type: &str) -> DiscoveredDevice {
    let host = info
        .get_addresses()
        .iter()
        .next()
        .map(|a| a.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let port = info.get_port();

    let name = info
        .get_property("name")
        .or_else(|| info.get_property("am"))
        .map(|v| v.val_str().to_string())
        .unwrap_or_else(|| info.get_fullname().to_string());

    let mut properties = HashMap::new();
    for prop in info.get_properties().iter() {
        properties.insert(prop.key().to_string(), prop.val_str().to_string());
    }

    let id = format!("{}:{}:{}", host, port, device_type);

    DiscoveredDevice {
        id,
        name,
        host: host.trim_start_matches('.').to_string(),
        port,
        device_type: device_type.to_string(),
        properties,
    }
}
