use std::time::Duration;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const READ_TIMEOUT: Duration = Duration::from_secs(60);

pub fn agent(read_timeout: Duration) -> ureq::Agent {
    agent_with_timeouts(CONNECT_TIMEOUT, read_timeout)
}

pub fn agent_with_timeouts(connect_timeout: Duration, read_timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(connect_timeout)
        .timeout_read(read_timeout)
        .timeout_write(read_timeout)
        .build()
}
