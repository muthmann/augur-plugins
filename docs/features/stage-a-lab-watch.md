# Windows lab watcher and iPhone notifications

## Scope

`Laborwache.exe` is a separate, read-only process. It does not change Augur, its
configuration, a recording, a serial port or a laser output. It sends generic
status text through ntfy.sh; it does not upload recordings, file paths, sample
names or full error messages.

It observes the selected RAW and PD recording folders every five seconds. Each
stream is checked independently. Sixty seconds without increasing file size sends
one warning for that stream. File rotation counts as progress; touching a file's
timestamp does not. Resumed progress sends one recovery message. A2 JSON sidecars
supply an additional warning when a finalized row reports an acquisition failure,
unconfirmed cleanup or an offline-review condition. Old sidecars present when the
watcher starts do not produce alerts.

This is a **file-progress alarm**, not a hardware-health or scientific-validity
certificate. It cannot reliably distinguish an operator pause, a completed protocol
and a hung application from file inactivity alone. It does not announce A1/A2
completion as proven, and it does not check trigger correspondence or PD signal
quality. For A1 it observes file progress only. A2 detailed errors appear once a
sidecar is written; otherwise the inactivity warning is the fallback.

## Start on the Windows lab computer

1. Download the newly built `augur-plugins-windows-x86_64` artifact from the approved
   GitHub build. Install its plugin folders into `%USERPROFILE%\.augur\plugins`
   with Augur closed. Keep the separate `lab-watch` folder outside the plugin folder.
2. On the iPhone, install **ntfy**, allow notifications, and permit them in the Focus
   mode used during the experiment. Keep the default server `https://ntfy.sh`.
3. Open `lab-watch/Laborwache.exe`. Subscribe in ntfy to the exact random topic shown
   by the program. The topic is saved locally for reuse. It is unguessable by design
   but not a password-protected topic; do not share it. Anyone who knows it can read
   or publish there. Messages therefore contain no research data.
4. Press Enter to send a test. Verify reception with the phone locked and on mobile
   data before confirming `ja`. Server acceptance alone is not delivery proof.
5. Choose the current PD output folder and the current RAW output folder. They can
   be the same folder. Select narrow session/day folders rather than an entire disk.
6. Start the watcher **before** the acquisition, then start the protocol. Leave its
   console open. Ctrl+C ends only the watcher. A missing-data alarm after a physical
   pause is expected: it asks you to inspect Augur; it does not stop a valid run.
7. Before leaving the desk, deliberately leave a small test capture idle long enough
   to receive the inactivity warning, then verify a recovery message when both files
   grow again. Do this as a recording test, not by disrupting a valuable acquisition.

## Failure boundaries

Network calls time out after eight seconds and retry pending messages after 30 s.
The watcher keeps scanning independently of Augur; a send failure never blocks the
recorders. The polling interval, scan time and a pending network request add latency
to the 60 s threshold. The console explicitly reports an unavailable warning channel.
An unsent stall message is replaced by recovery if the data resumes before delivery.

A computer power failure, Windows sleep, closed watcher, full network outage or
failure of the notification service can prevent every local warning. This package
has **no external missed-heartbeat service**. Do not call it protection against
those failures or a reason to leave an optically unsafe setup unattended. Use short
blocks and local laboratory rules; an independently hosted heartbeat receiver is a
separate requirement for full remote outage detection.

The topic path is stored under `%LOCALAPPDATA%\AugurLabWatch\topic.txt`. There is no
remote start/stop function. The watcher does not install itself as a service or
change Windows power, firewall or global script-execution settings.

## Validation and build

Run `python scripts/test_stage_a_watch.py` to test file progression, independent
streams, file rotation, alert deduplication, invalid/incomplete sidecars, unreachable
folders and notification retry behaviour. The tests never send a real push message.
GitHub packages the Windows executable with Python 3.12 and PyInstaller 6.16.0. The
Windows package and iPhone delivery still need their own build/receipt verification;
passing local Python tests does not establish either.

Primary service documentation: [ntfy phone subscriptions](https://docs.ntfy.sh/subscribe/phone/)
and [publishing messages](https://docs.ntfy.sh/publish/).
