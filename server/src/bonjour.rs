use mdns_sd::{ServiceDaemon, ServiceInfo};

use crate::{BRAND_BONJOUR_TYPE, BRAND_NAME, BRAND_SLUG};

pub fn advertise(port: u16, server_id: &str) -> Option<ServiceDaemon> {
    let result = (|| -> Result<ServiceDaemon, mdns_sd::Error> {
        let daemon = ServiceDaemon::new()?;
        let host = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .unwrap_or_else(|_| format!("{BRAND_SLUG}-{}", &server_id[..8]));
        let host = host.split('.').next().unwrap_or(BRAND_SLUG).to_string();
        let props = [("v", "1"), ("path", "/v1"), ("id", server_id)];
        let info = ServiceInfo::new(
            &format!("{BRAND_BONJOUR_TYPE}.local."),
            &format!("{BRAND_NAME} ({host})"),
            &format!("{host}.local."),
            "",
            port,
            &props[..],
        )?
        .enable_addr_auto();
        daemon.register(info)?;
        Ok(daemon)
    })();
    match result {
        Ok(d) => Some(d),
        Err(e) => {
            tracing::warn!("bonjour advertisement failed: {e}");
            None
        }
    }
}
