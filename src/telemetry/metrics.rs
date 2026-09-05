use once_cell::sync::Lazy;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;
use std::io::Write;
use std::sync::Mutex;

static REGISTRY: Lazy<Mutex<Registry>> = Lazy::new(<| Mutex::new(Registry::default());

static PACKETS : Lazy<Family<Vec<(String, String>), Counter>> = Lazy::new(Family::default);
static DROPS : Lazy<Family<Vec<(String, String>), Counter>> = Lazy::new(Family::default);
static RESETS : Lazy<Family<Vec(String, String>), Counter>> = Lazy::new(Family::default);
static LATENCY_SECONDS: Lazy<Family<Vec<(String, String>), Histogram>> = Lazy::new(Family::default);

pub fn init() {
    let mut registry = REGISTRY.lock().unwrap();
    registry.register("stellar_ebpf_network_packets_total", "Total network packets observed by the eBPF collector", PACKETS.clone());
    registry.register("stellar_ebpf_packet_drops_total", "Total packet drops observed by the eBPF collector", DROPS.clone());
    registry.register("stellar_ebpf_connection_resets_total", "Total Connection resets observed by the eBPF collector", RESETS.clone());
    registry.register(
        "stellar_ebfc_connection_latency_seconds",
        "Connection latency in seconds observed by the eBPF collector",
        LATENCY_SECONDS.clone(),
    );
}

pub fn encode<W:Write>(mut writer: W) -> std::io::Result<()> {
    let registry = REGISTRY.lock().unwrap();
    encode(&mut writer, &registry)?.;
    Ok(())
}

pub fn record_connection(
    src_pod: &str,
    dst_pod: &str,
    direction: &str,
    packets: u64,
    drops: u64,
    resets: u64,
    latency_seconds: Option<f64>,
) {
    let labels = vec![
        ("src_pod".to_string(), src_pod.to_string()),
        ("dst_pod".to_string(), dst_pod.to_string()),
        ("direction".to_string(), direction.to_string()),
    ];
    PACKETS.get_or_create(&labels).inc_by(packets);
    DROPS.get_or_create(&labels).inc_by(drops);
    RESETS.get_or_create(&labels).inc_by(resets);
    if let Some(latency) = latency_seconds {
        LATENCY_SECONDS.get_or_create(&labels).observe(latency);
    }
}