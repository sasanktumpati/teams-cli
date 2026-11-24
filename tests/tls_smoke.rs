use std::time::Duration;

#[test]
#[ignore = "needs network access"]
fn https_handshake_with_the_system_trust_store() {
    let agent = teams_cli::live::http::agent(Duration::from_secs(20));
    for url in ["https://login.microsoftonline.com/", "https://teams.microsoft.com/"] {
        match agent.head(url).call() {
            Ok(_) | Err(ureq::Error::Status(_, _)) => {}
            Err(e) => panic!("{url}: {e}"),
        }
    }
}
