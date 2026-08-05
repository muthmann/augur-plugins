//! Bounded background I/O worker.
//!
//! The owning plugin's `process_frame()` must never block on serial: it only
//! drains this worker's bounded output queue and pushes bounded requests.
//! The worker thread owns the [`StageAClient`] (and thereby the serial
//! port), sends `PING` at 2 Hz while the controller is armed/running, and
//! requests `STOP` on shutdown. Firmware safety does not depend on that
//! STOP arriving — the on-device watchdog falls back to `SAFE_IDLE` — but a
//! clean stop is always attempted.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::client::{ClientError, DeviceEvent, StageAClient, StreamIntegrity};
use crate::protocol::Command;
use crate::transport::Transport;

pub const COMMAND_QUEUE_DEPTH: usize = 16;
pub const OUTPUT_QUEUE_DEPTH: usize = 256;
const PING_INTERVAL: Duration = Duration::from_millis(500);
const IDLE_POLL: Duration = Duration::from_millis(5);

/// Requests the plugin can queue for the worker.
#[derive(Debug, Clone)]
pub enum WorkerRequest {
    /// Send a command and report its reply (or error) as a `Reply` output.
    Send { tag: u64, command: Command },
    /// Enable/disable the 2 Hz watchdog ping (armed/running phases).
    SetPinging(bool),
    /// Stop the controller and shut the worker down.
    Shutdown { reason: String },
}

/// Bounded outputs the plugin drains from `process_frame()`.
#[derive(Debug)]
pub enum WorkerOutput {
    Reply {
        tag: u64,
        result: Result<BTreeMap<String, String>, String>,
    },
    Event(DeviceEvent),
    Integrity(StreamIntegrity),
    /// The worker exited (clean shutdown or transport failure).
    Stopped {
        reason: String,
    },
}

pub struct IoWorker {
    requests: SyncSender<WorkerRequest>,
    outputs: Receiver<WorkerOutput>,
    join: Option<JoinHandle<()>>,
}

impl IoWorker {
    /// Spawns the worker over an already-open transport. Opening the
    /// transport (and failing visibly if the device is busy) is the
    /// caller's responsibility, in `LiveCapture` with effects allowed only.
    pub fn spawn<T: Transport + 'static>(client: StageAClient<T>) -> Self {
        let (request_tx, request_rx) = std::sync::mpsc::sync_channel(COMMAND_QUEUE_DEPTH);
        let (output_tx, output_rx) = std::sync::mpsc::sync_channel(OUTPUT_QUEUE_DEPTH);
        let join = std::thread::Builder::new()
            .name("stage-a-io".into())
            .spawn(move || run_worker(client, request_rx, output_tx))
            .expect("spawning the stage-a I/O thread must succeed");
        Self {
            requests: request_tx,
            outputs: output_rx,
            join: Some(join),
        }
    }

    /// Non-blocking enqueue; a full queue is a visible error, not a stall.
    pub fn try_send(&self, request: WorkerRequest) -> Result<(), String> {
        self.requests.try_send(request).map_err(|err| match err {
            TrySendError::Full(_) => "stage-a I/O command queue is full".to_owned(),
            TrySendError::Disconnected(_) => "stage-a I/O worker is gone".to_owned(),
        })
    }

    /// Drains everything currently queued, without blocking.
    pub fn drain_outputs(&self) -> Vec<WorkerOutput> {
        let mut out = Vec::new();
        while let Ok(output) = self.outputs.try_recv() {
            out.push(output);
        }
        out
    }

    /// Requests a controller STOP and joins the worker.
    pub fn shutdown(mut self, reason: &str) {
        let _ = self.requests.try_send(WorkerRequest::Shutdown {
            reason: reason.to_owned(),
        });
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for IoWorker {
    fn drop(&mut self) {
        let _ = self.requests.try_send(WorkerRequest::Shutdown {
            reason: "worker dropped".to_owned(),
        });
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run_worker<T: Transport>(
    mut client: StageAClient<T>,
    requests: Receiver<WorkerRequest>,
    outputs: SyncSender<WorkerOutput>,
) {
    let mut pinging = false;
    let mut last_ping = Instant::now();
    let mut last_integrity = client.integrity();

    let stop_reason = loop {
        match requests.recv_timeout(IDLE_POLL) {
            Ok(WorkerRequest::Send { tag, command }) => {
                let result = client
                    .request(&command)
                    .map_err(|err: ClientError| err.to_string());
                if outputs
                    .try_send(WorkerOutput::Reply { tag, result })
                    .is_err()
                {
                    break "output queue closed".to_owned();
                }
            }
            Ok(WorkerRequest::SetPinging(enabled)) => {
                pinging = enabled;
                last_ping = Instant::now();
            }
            Ok(WorkerRequest::Shutdown { reason }) => break reason,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break "request queue closed".to_owned(),
        }

        match client.poll_events() {
            Ok(events) => {
                for event in events {
                    // Bounded best-effort delivery: a full output queue drops
                    // live telemetry, never blocks the serial loop. Exact
                    // data is preserved by the PDQ writer downstream of the
                    // worker owner, which uses Reply-driven flow instead.
                    let _ = outputs.try_send(WorkerOutput::Event(event));
                }
            }
            Err(err) => {
                let _ = outputs.try_send(WorkerOutput::Reply {
                    tag: 0,
                    result: Err(err.to_string()),
                });
                break "transport failure".to_owned();
            }
        }

        let integrity = client.integrity();
        if integrity != last_integrity {
            last_integrity = integrity;
            let _ = outputs.try_send(WorkerOutput::Integrity(integrity));
        }

        if pinging && last_ping.elapsed() >= PING_INTERVAL {
            last_ping = Instant::now();
            let _ = client.request(&Command::new("PING"));
        }
    };

    // Best-effort clean stop; the firmware watchdog is the real guarantee.
    let _ = client.request(&Command::new("STOP").field("reason", stop_reason.replace(' ', "_")));
    let _ = outputs.try_send(WorkerOutput::Stopped {
        reason: stop_reason,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockController;
    use crate::transport::MockLink;

    #[test]
    fn worker_round_trips_commands_and_stops_cleanly() {
        let link = MockLink::new();
        let mut controller = MockController::new(link.device_end());
        let client =
            StageAClient::new(link.host_end()).with_reply_timeout(Duration::from_millis(100));
        let worker = IoWorker::spawn(client);

        // HELLO via the worker, served by the mock on this thread.
        worker
            .try_send(WorkerRequest::Send {
                tag: 1,
                command: Command::new("HELLO").field("protocol", 1),
            })
            .expect("enqueue");
        controller.serve_n_commands(1);

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut reply_seen = false;
        while Instant::now() < deadline && !reply_seen {
            for output in worker.drain_outputs() {
                if let WorkerOutput::Reply { tag: 1, result } = output {
                    let fields = result.expect("HELLO succeeds");
                    assert_eq!(fields.get("protocol").map(String::as_str), Some("1"));
                    reply_seen = true;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(reply_seen, "HELLO reply must reach the plugin queue");

        // Shutdown must send STOP to the controller.
        let handle = std::thread::spawn(move || {
            controller.serve_n_commands(1);
            controller
        });
        worker.shutdown("test done");
        let controller = handle.join().expect("mock joins");
        assert_eq!(controller.state(), crate::mock::MockState::SafeIdle);
    }
}
