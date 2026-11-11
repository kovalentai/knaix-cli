//! Listing nodes, their health and logs, and starting one, run the way a
//! script runs them: through the binary, against a fake control plane or a
//! fake local node on loopback, with no terminal.

mod common;

use common::{knaix, scratch_home, server};
use serde_json::json;

const PROVISION: &str = "POST /api/instances";

#[test]
fn up_refused_exits_with_the_code_its_status_means() {
    let home = scratch_home("nodes-up-refused");
    for (status, code) in [(401, 3), (403, 3), (402, 1)] {
        let api = server(vec![(
            PROVISION,
            status,
            json!({"error": "no"}).to_string(),
        )]);

        let up = knaix(&home, &api.url, &["up"]);

        assert_eq!(up.code, code, "HTTP {status}: {}", up.err);
        assert!(
            up.err.contains("Failed to provision node: HTTP"),
            "{}",
            up.err
        );
        assert!(
            up.err.contains(" - no"),
            "the control plane's reason: {}",
            up.err
        );
    }
}
