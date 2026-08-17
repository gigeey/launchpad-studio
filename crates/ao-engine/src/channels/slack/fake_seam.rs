//! In-memory [`SlackApiSeam`] fake — the Slack Test Connection analogue of
//! `ao_process::mock::MockProcessSupervisor`. Returns scripted results
//! instead of calling `slack.com`, so both this crate's own tests
//! ([`super::test_connection`]) and `ao-server`'s integration tests can
//! exercise the full Test Connection flow hermetically. Exported
//! unconditionally (not `#[cfg(test)]`-gated) for exactly that
//! cross-crate reuse, mirroring `MockProcessSupervisor`'s own visibility.

use std::sync::Mutex;

use async_trait::async_trait;

use super::web_api_seam::{SlackApiCallError, SlackApiSeam, SlackAuthTestResult};

pub struct FakeSlackApiSeam {
    auth_test_result: Result<SlackAuthTestResult, SlackApiCallError>,
    connections_open_result: Result<(), SlackApiCallError>,
    /// Scripted outcome for [`SlackApiSeam::post_message`] — defaults to
    /// always succeeding (see [`Self::new`]) since every current caller of
    /// this fake only exercises Test Connection's two checks above. Not a
    /// constructor parameter, so adding it doesn't disturb the existing
    /// `new`/`all_checks_pass` call sites in `ao-server`'s and this crate's
    /// own Test Connection tests.
    post_message_result: Result<(), SlackApiCallError>,
    /// `(channel, thread_ts, text)` for every `post_message` call, in order
    /// — lets an outbound-relay test assert what was actually sent.
    post_message_calls: Mutex<Vec<(String, Option<String>, String)>>,
}

impl FakeSlackApiSeam {
    /// Scripts both calls independently — a bad app token failing
    /// `connections_open` while `auth_test` still succeeds (or vice versa)
    /// is exactly the scenario Test Connection's per-check reporting exists
    /// to surface.
    pub fn new(
        auth_test_result: Result<SlackAuthTestResult, SlackApiCallError>,
        connections_open_result: Result<(), SlackApiCallError>,
    ) -> Self {
        Self {
            auth_test_result,
            connections_open_result,
            post_message_result: Ok(()),
            post_message_calls: Mutex::new(Vec::new()),
        }
    }

    /// Convenience constructor for the fully-healthy case.
    pub fn all_checks_pass(identity: SlackAuthTestResult) -> Self {
        Self::new(Ok(identity), Ok(()))
    }

    /// Scripts `post_message` to fail instead of the default success —
    /// builder-style so it composes with either constructor above without
    /// adding a third positional argument to both.
    pub fn with_post_message_result(mut self, result: Result<(), SlackApiCallError>) -> Self {
        self.post_message_result = result;
        self
    }

    /// Every `post_message` call recorded so far, in order.
    pub fn post_message_calls(&self) -> Vec<(String, Option<String>, String)> {
        self.post_message_calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[async_trait]
impl SlackApiSeam for FakeSlackApiSeam {
    async fn auth_test(&self, _bot_token: &str) -> Result<SlackAuthTestResult, SlackApiCallError> {
        self.auth_test_result.clone()
    }

    async fn connections_open(&self, _app_token: &str) -> Result<(), SlackApiCallError> {
        self.connections_open_result.clone()
    }

    async fn post_message(
        &self,
        _bot_token: &str,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
    ) -> Result<(), SlackApiCallError> {
        self.post_message_calls.lock().unwrap_or_else(|e| e.into_inner()).push((
            channel.to_string(),
            thread_ts.map(str::to_string),
            text.to_string(),
        ));
        self.post_message_result.clone()
    }
}
