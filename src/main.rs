use std::cmp::min;
use std::env;
use std::fmt::Debug;
use std::fs;
use std::io::ErrorKind::NotFound;
use std::io::Write;
use std::sync::mpsc::channel;
use std::time::{Duration, UNIX_EPOCH};

use futures::stream;
use influxdb2::api::write::TimestampPrecision;
use influxdb2::models::{DataPoint, WriteDataPoint};
use influxdb2::Client as InfluxClient;
use log::{debug, error, info, LevelFilter};
use serde::{Deserialize, Serialize};
use simple_logger::SimpleLogger;
use tapo::responses::EnergyUsageResult;
use tapo::{ApiClient, Authenticated, P110Handler};
use tokio::time;

const DEFAULT_CONFIG_PATH: &str = "./config.toml";
const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);
const DEFAULT_REPORT_BATCH_SIZE: usize = 5;

static mut RUNNING: bool = true;

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Config {
    tapo: TapoConfig,
    db: DbConfig,
    report_batch_size: Option<usize>,
    log_level: Option<LogLevel>,
    log_lib: Option<bool>,
}

impl Config {
    fn get_influx_client(&self) -> InfluxClient {
        InfluxClient::new(
            self.db.influx_host.clone(),
            self.db.influx_org.clone(),
            self.db.influx_token.clone(),
        )
    }

    fn get_clients(&self) -> Vec<Device> {
        let client_creds: Vec<_> = self
            .tapo
            .clients
            .iter()
            .map(|client| {
                return (
                    client.ip.clone(),
                    client
                        .credentials
                        .clone()
                        .or_else(|| self.tapo.default_credentials.clone())
                        .expect("No Credentials for client"),
                    client
                        .interval
                        .or(self.tapo.default_interval)
                        .map(Duration::from_secs),
                );
            })
            .collect();

        let mut clients = Vec::new();
        for (ip, credentials, interval) in client_creds.into_iter() {
            let client = Device::new(ip.clone(), credentials, interval);
            clients.push(client)
        }

        clients
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tapo: TapoConfig {
                default_credentials: Some(TapoCredentials {
                    username: "username".to_owned(),
                    password: "password".to_owned(),
                }),
                default_interval: Some(5),
                clients: vec![
                    ClientConfig {
                        ip: "192.168.178.101".to_owned(),
                        credentials: None,
                        interval: None,
                    },
                    ClientConfig {
                        ip: "192.168.178.102".to_owned(),
                        credentials: Some(TapoCredentials {
                            username: "username".to_owned(),
                            password: "password102".to_owned(),
                        }),
                        interval: Some(60),
                    },
                    ClientConfig {
                        ip: "192.168.178.103".to_owned(),
                        credentials: Some(TapoCredentials {
                            username: "username".to_owned(),
                            password: "password103".to_owned(),
                        }),
                        interval: None,
                    },
                ],
            },
            log_level: Some(LogLevel::Info),
            db: DbConfig {
                influx_host: "https://yourdatacenter.influxdata.com".to_owned(),
                influx_org: "org".to_owned(),
                influx_token: "token".to_owned(),
                influx_bucket: "bucket".to_owned(),
            },
            report_batch_size: Some(10),
            log_lib: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct DbConfig {
    influx_host: String,
    influx_org: String,
    influx_token: String,
    influx_bucket: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct TapoConfig {
    default_credentials: Option<TapoCredentials>,
    default_interval: Option<u64>,
    clients: Vec<ClientConfig>,
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<LogLevel> for LevelFilter {
    fn from(value: LogLevel) -> Self {
        match value {
            LogLevel::Off => Self::Off,
            LogLevel::Error => Self::Error,
            LogLevel::Warn => Self::Warn,
            LogLevel::Info => Self::Info,
            LogLevel::Debug => Self::Debug,
            LogLevel::Trace => Self::Trace,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct TapoCredentials {
    username: String,
    password: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ClientConfig {
    ip: String,
    credentials: Option<TapoCredentials>,
    interval: Option<u64>,
}

#[tokio::main]
async fn main() {
    let config_path = &env::args()
        .skip(1)
        .next()
        .unwrap_or(DEFAULT_CONFIG_PATH.to_owned());

    let config = match fs::read_to_string(config_path) {
        Ok(config_string) => toml::from_str::<Config>(&config_string),
        Err(e) => {
            if e.kind() == NotFound {
                fs::write(
                    config_path,
                    if cfg!(debug_assertions) {
                        toml::to_string(&Config::default())
                            .expect("Could not serialize default config!")
                    } else {
                        include_str!("example-config.toml").to_owned()
                    },
                )
                .unwrap_or_else(|_| panic!("Could not write {config_path} config!"));
            }
            panic!("Could not read {config_path}!")
        }
    }
    .unwrap_or_else(|e| {
        panic!("Unable to parse config.toml: {e}");
    });

    debug!("Config parsed: {config:#?}");

    let level_filter = config.log_level.unwrap_or(LogLevel::Info).into();

    if config.log_lib.unwrap_or(false) {
        SimpleLogger::new().with_level(level_filter)
    } else {
        SimpleLogger::new()
            .with_level(min(level_filter, LevelFilter::Info))
            .with_module_level(module_path!(), level_filter)
    }
    .init()
    .expect("Could not initialize logger!");

    debug!("Logger initialized. Level: {}", level_filter);

    let influx_client = config.get_influx_client();
    debug!("Influx Client instantiated");

    let clients = config.get_clients();
    debug!("Instantiated {} Tapo clients", clients.len());

    ctrlc::set_handler(stop).expect("Error setting Ctrl-C handler!");

    debug!("Processing clients... (CTRL-C to stop execution)");
    process_clients(
        influx_client,
        config.db.influx_bucket,
        config
            .report_batch_size
            .unwrap_or(DEFAULT_REPORT_BATCH_SIZE),
        clients,
    )
    .await;

    info!("Terminating");
}

async fn process_clients(
    influx_client: InfluxClient,
    bucket: String,
    batch_size: usize,
    clients: Vec<Device>,
) {
    let (tx, rx) = channel();

    for mut client in clients {
        let tx = tx.clone();

        tokio::spawn(async move {
            let mut interval = time::interval(client.interval.unwrap_or(DEFAULT_INTERVAL));

            loop {
                interval.tick().await;

                match client.record_data().await {
                    Ok(measurement) => {
                        debug!("Measurement recorded: {:?}", measurement);
                        tx.send(measurement)
                            .expect("Could not send measurement to main thread!");
                    }
                    Err(e) => {
                        error!("Could not record measurement! {}", e);
                        // match e {
                        //     tapo::Error::Tapo(_) => {}
                        //     tapo::Error::Validation { .. } => {}
                        //     tapo::Error::Serde(_) => {}
                        //     tapo::Error::Http(_) => {
                        //         // sleep(Duration::from_secs(30)).await; // if not reachable wait...
                        //     }
                        //     tapo::Error::Other(_) => {}
                        //     _ => {}
                        // }
                    }
                };

                if !is_running() {
                    break;
                }
            }
        });
    }

    let mut buf = Vec::new();

    for measurement in rx {
        buf.push(measurement);

        if buf.len() >= batch_size || !is_running() {
            debug!("Writing batch of {} data-points", buf.len());

            let buf2 = buf;
            buf = Vec::new();

            influx_client
                .write_with_precision(&bucket, stream::iter(buf2), TimestampPrecision::Seconds)
                .await
                .expect("Could not write to database!");
        }

        if !is_running() {
            break;
        }
    }
}

fn stop() {
    unsafe { RUNNING = false };
}

fn is_running() -> bool {
    unsafe { RUNNING }
}

#[derive(Debug)]
struct Measurement {
    device: String,
    current_power: f64,
    timestamp: i64,
}

impl Measurement {
    const fn new(device: String, current_power: f64, timestamp: i64) -> Self {
        Self {
            device,
            current_power,
            timestamp,
        }
    }

    fn from_result(eur: &EnergyUsageResult, device: String) -> Self {
        Self::new(
            device,
            eur.current_power as f64 / 1000.,
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .try_into()
                .expect("Could not convert timestamp from u64 to i64"),
        )
    }
}

impl WriteDataPoint for Measurement {
    fn write_data_point_to<W>(&self, w: W) -> std::io::Result<()>
    where
        W: Write,
    {
        let d: DataPoint = self.into();
        d.write_data_point_to(w)
    }
}

impl From<&Measurement> for DataPoint {
    fn from(m: &Measurement) -> Self {
        Self::builder("power_consumption")
            .tag("device_name", &m.device)
            .field("power", m.current_power)
            .timestamp(m.timestamp)
            .build()
            .unwrap()
    }
}

struct Client {
    handler: P110Handler<Authenticated>,
    device_id: String,
}

impl Client {
    async fn new(ip: String, credentials: TapoCredentials) -> Result<Client, tapo::Error> {
        let handler = ApiClient::new(ip, credentials.username, credentials.password)?.p110();
        let handler = handler.login().await?;
        let device_info = handler.get_device_info().await?;
        let device_id = device_info.device_id;
        Ok(Client { handler, device_id })
    }
}

struct Device {
    ip: String,
    credentials: TapoCredentials,
    client: Option<Client>,
    interval: Option<Duration>,
}

impl Device {
    fn new(ip: String, credentials: TapoCredentials, interval: Option<Duration>) -> Self {
        Self {
            ip,
            credentials,
            client: None,
            interval,
        }
    }

    async fn record_data(&mut self) -> Result<Measurement, tapo::Error> {
        let client = if let Some(client) = &self.client {
            client
        } else {
            let client = Client::new(self.ip.clone(), self.credentials.clone()).await?;
            self.client = Some(client);
            self.client.as_ref().unwrap()
        };

        let energy_usage = client.handler.get_energy_usage().await?;
        Ok(Measurement::from_result(
            &energy_usage,
            client.device_id.clone(),
        ))
    }
}
