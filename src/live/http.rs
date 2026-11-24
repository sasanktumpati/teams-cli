use std::time::Duration;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const READ_TIMEOUT: Duration = Duration::from_secs(60);

pub fn agent(read_timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(read_timeout)
        .timeout_write(read_timeout)
        .build()
}
