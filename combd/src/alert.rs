//! Alerts: `combd watch` tells someone when a node stops answering.
//!
//! Cron runs it every minute on each node. A node watches the one after it
//! (`COMBD_PUBLIC_URLS`; a node alone watches itself), from outside the
//! serving process, so a combd that can't start is still seen. Three
//! misses in a row is down: an alert then, one every hour while it lasts,
//! and one when the node answers again. (2026-11: a fleet whose nodes
//! couldn't start was down for 11 days, and nothing said so.)
//!
//! Where alerts go: the file named by `COMBD_ALERTS` (it holds keys: mode
//! 600), one place a line, `#` for comments:
//!
//! ```text
//! forwardemail <api key> <from> <to>
//! slack <webhook url>
//! pagerduty <routing key>
//! webhook <url>
//! ```
//!
//! `combd watch test` sends a test alert to each. Without `COMBD_ALERTS`,
//! `combd watch` does nothing.

use crate::admin::fetch;
use serde_json::json;
use std::time::Duration;

/// Misses in a row before a node is down: a restart for an update is one.
const MISSES: u32 = 3;
/// How often to say it's still down.
const AGAIN_S: u64 = 3600;

#[derive(Debug, PartialEq)]
pub enum Sink {
    ForwardEmail { key: String, from: String, to: String },
    Slack(String),
    PagerDuty(String),
    Webhook(String),
}

pub struct Alert {
    /// The same for a node's down and its up: PagerDuty closes by it.
    pub key: String,
    pub up: bool,
    pub subject: String,
    pub text: String,
}

pub fn parse(file: &str) -> Result<Vec<Sink>, String> {
    let mut out = vec![];
    for l in file.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let w: Vec<&str> = l.split_whitespace().collect();
        let url = |u: &str| if u.starts_with("https://") || u.starts_with("http://") { Ok(u.to_string()) } else { Err(format!("{}: want a URL, not {u:?}", w[0])) };
        out.push(match w[..] {
            ["forwardemail", key, from, to] => Sink::ForwardEmail { key: key.into(), from: from.into(), to: to.into() },
            ["slack", u] => Sink::Slack(url(u)?),
            ["pagerduty", key] => Sink::PagerDuty(key.into()),
            ["webhook", u] => Sink::Webhook(url(u)?),
            // Not the line: it may hold a key.
            _ => return Err(format!("{}: want `forwardemail <key> <from> <to>`, `slack <url>`, `pagerduty <key>`, or `webhook <url>`", w[0])),
        });
    }
    if out.is_empty() {
        return Err("no place to send alerts".into());
    }
    Ok(out)
}

impl Sink {
    fn name(&self) -> &'static str {
        match self {
            Sink::ForwardEmail { .. } => "forwardemail",
            Sink::Slack(_) => "slack",
            Sink::PagerDuty(_) => "pagerduty",
            Sink::Webhook(_) => "webhook",
        }
    }

    /// The POST that tells this place: (url, headers, body).
    fn request(&self, a: &Alert) -> (String, String, String) {
        let json_to = |url: &str, v: serde_json::Value| (url.to_string(), "content-type: application/json\r\n".to_string(), v.to_string());
        match self {
            Sink::ForwardEmail { key, from, to } => {
                let enc = |s: &str| crate::s3::enc(s, false);
                let head = format!("authorization: Basic {}\r\ncontent-type: application/x-www-form-urlencoded\r\n", crate::ws::base64(format!("{key}:").as_bytes()));
                ("https://api.forwardemail.net/v1/emails".into(), head, format!("from={}&to={}&subject={}&text={}", enc(from), enc(to), enc(&a.subject), enc(&a.text)))
            }
            Sink::Slack(url) => json_to(url, json!({ "text": format!("*{}*\n{}", a.subject, a.text) })),
            Sink::PagerDuty(key) => json_to(
                "https://events.pagerduty.com/v2/enqueue",
                json!({
                    "routing_key": key,
                    "event_action": if a.up { "resolve" } else { "trigger" },
                    "dedup_key": a.key,
                    "payload": { "summary": a.subject, "source": a.key, "severity": "critical", "custom_details": { "text": a.text } },
                }),
            ),
            Sink::Webhook(url) => json_to(url, json!({ "key": a.key, "state": if a.up { "up" } else { "down" }, "subject": a.subject, "text": a.text })),
        }
    }

    fn send(&self, a: &Alert) -> Result<(), String> {
        let (url, headers, body) = self.request(a);
        match fetch("POST", &url, &headers, body.as_bytes(), Duration::from_secs(10))? {
            (200..=299, _) => Ok(()),
            (status, body) => Err(format!("HTTP {status}: {}", body.chars().take(200).collect::<String>())),
        }
    }
}

/// Send `a` everywhere. True if it reached at least one place.
pub fn tell(sinks: &[Sink], a: &Alert) -> bool {
    let mut told = false;
    for s in sinks {
        match s.send(a) {
            Ok(()) => told = true,
            Err(e) => eprintln!("combd     watch: {} didn't take the alert: {e}", s.name()),
        }
    }
    told
}

/// What a watcher remembers between runs. All 0: the node is up.
#[derive(Debug, Default, PartialEq, Clone, Copy)]
struct State {
    /// Checks missed in a row.
    misses: u32,
    /// When the first of them was (Unix seconds).
    since: u64,
    /// When we last said it's down (0: not yet).
    told: u64,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum What {
    Down,
    Still,
    Up,
}

/// One check's result: the new state, and what to say, if anything. The
/// caller sets `told` once a Down or Still is sent, and keeps the old state
/// if an Up isn't, so either is tried again on the next run.
fn step(st: State, ok: bool, now: u64) -> (State, Option<What>) {
    if ok {
        return (State::default(), (st.told > 0).then_some(What::Up));
    }
    let st = State { misses: st.misses + 1, since: if st.misses == 0 { now } else { st.since }, told: st.told };
    let what = match st.told {
        _ if st.misses < MISSES => None,
        0 => Some(What::Down),
        t if now.saturating_sub(t) >= AGAIN_S => Some(What::Still),
        _ => None,
    };
    (st, what)
}

fn lasted(s: u64) -> String {
    let (n, unit) = match s {
        0..=5399 => ((s / 60).max(1), "minute"),
        5400..=172_799 => ((s + 1800) / 3600, "hour"),
        _ => ((s + 43_200) / 86_400, "day"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// `combd watch`: check the node after this one, and say what changed.
pub fn watch(test: bool) -> Result<(), String> {
    let Ok(file) = std::env::var("COMBD_ALERTS") else {
        return if test { Err("set COMBD_ALERTS to the file that says where alerts go".into()) } else { Ok(()) };
    };
    let sinks = parse(&std::fs::read_to_string(&file).map_err(|e| format!("{file}: {e}"))?).map_err(|e| format!("{file}: {e}"))?;
    let me: u32 = std::env::var("COMBD_NODE").ok().and_then(|n| n.parse().ok()).unwrap_or(0);
    if test {
        let a = Alert { key: format!("combd test from node {me}"), up: false, subject: format!("combd: a test alert from node {me}"), text: "Sent by `combd watch test`. Nothing is down.".into() };
        let bad: Vec<String> = sinks.iter().filter_map(|s| s.send(&a).err().map(|e| format!("{}: {e}", s.name()))).collect();
        // PagerDuty opened an incident for it: close it.
        let _ = tell(&sinks.into_iter().filter(|s| matches!(s, Sink::PagerDuty(_))).collect::<Vec<_>>(), &Alert { up: true, ..a });
        return if bad.is_empty() { Ok(println!("combd     watch: the test alert went everywhere")) } else { Err(bad.join("; ")) };
    }
    let cfg = crate::http::Config::from_env();
    let (n, url) = cfg.public.range(me + 1..).next().or(cfg.public.iter().next()).ok_or("no node to watch: set COMBD_PUBLIC_URLS")?;
    let url = url.trim_end_matches('/');
    let state = cfg.data.join("watch");
    let was = std::fs::read_to_string(&state).ok().and_then(|s| {
        let mut w = s.split_whitespace().map(str::parse::<u64>);
        Some(State { misses: w.next()?.ok()? as u32, since: w.next()?.ok()?, told: w.next()?.ok()? })
    });
    let was = was.unwrap_or_default();
    let now = crate::host::now_ms() as u64 / 1000;
    let (ok, why) = match fetch("GET", &format!("{url}/_combd/ping"), "", b"", Duration::from_secs(5)) {
        Ok((200, _)) => (true, String::new()),
        Ok((status, _)) => (false, format!("HTTP {status}")),
        Err(e) => (false, e),
    };
    let (mut st, what) = step(was, ok, now);
    if let Some(what) = what {
        let down = lasted(now.saturating_sub(was.since.max(st.since)));
        let (subject, text) = match what {
            What::Down => (format!("combd: node {n} is down"), format!("{url} has not answered for {down} ({why}). Seen from node {me}. This comes every hour until it answers.")),
            What::Still => (format!("combd: node {n} is still down, {down}"), format!("{url} has not answered for {down} ({why}). Seen from node {me}.")),
            What::Up => (format!("combd: node {n} is back up"), format!("{url} answers again, after {down}. Seen from node {me}.")),
        };
        match (tell(&sinks, &Alert { key: format!("combd node {n} {url}"), up: ok, subject, text }), ok) {
            (true, false) => st.told = now,
            (false, true) => st = was,
            _ => {}
        }
    }
    if st != was {
        std::fs::write(&state, format!("{} {} {}\n", st.misses, st.since, st.told)).map_err(|e| format!("{}: {e}", state.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Down after three misses, again each hour, up once, and nothing for a
    /// blip; an alert that didn't go out is tried again.
    #[test]
    fn down_hourly_and_up() {
        let mut st = State::default();
        let mut said = vec![];
        // (seconds, did the node answer, did the alert go out)
        let runs = [(0, false, true), (60, false, true), (120, false, false), (180, false, true), (240, false, true), (3720, false, true), (3780, false, true), (7380, false, true), (7440, true, false), (7500, true, true), (7560, true, true), (7620, false, true), (7680, true, true)];
        for (now, ok, sent) in runs {
            let was = st;
            let (next, what) = step(st, ok, now);
            st = next;
            if let Some(w) = what {
                said.push((now, w, sent));
                match (sent, ok) {
                    (true, false) => st.told = now,
                    (false, true) => st = was,
                    _ => {}
                }
            }
        }
        use What::*;
        assert_eq!(said, [(120, Down, false), (180, Down, true), (3780, Still, true), (7380, Still, true), (7440, Up, false), (7500, Up, true)]);
        assert_eq!(st, State::default());
    }

    #[test]
    fn the_file_and_each_request() {
        let sinks = parse("# where alerts go\nforwardemail k3y alerts@example.com me@example.com\n\nslack https://hooks.slack.com/services/T/B/x\npagerduty r0ute\nwebhook http://10.0.0.5/hook\n").unwrap();
        assert_eq!(sinks.len(), 4);
        for bad in ["", "# nothing", "slack hooks.slack.com/x", "email a b c", "forwardemail k3y from"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert!(!parse("forwardemail s3cret from").unwrap_err().contains("s3cret"), "a bad line's key isn't printed");
        let a = Alert { key: "combd node 1 https://n1".into(), up: false, subject: "combd: node 1 is down".into(), text: "no answer & more".into() };
        let (url, head, body) = sinks[0].request(&a);
        assert_eq!(url, "https://api.forwardemail.net/v1/emails");
        assert!(head.contains("authorization: Basic azN5Og==\r\n"), "{head}");
        assert_eq!(body, "from=alerts%40example.com&to=me%40example.com&subject=combd%3A%20node%201%20is%20down&text=no%20answer%20%26%20more");
        let json = |i: usize, a: &Alert| serde_json::from_str::<serde_json::Value>(&sinks[i].request(a).2).unwrap();
        assert_eq!(json(1, &a)["text"], "*combd: node 1 is down*\nno answer & more");
        assert_eq!((json(2, &a)["event_action"].as_str(), json(2, &a)["routing_key"].as_str()), (Some("trigger"), Some("r0ute")));
        let up = Alert { up: true, ..a };
        assert_eq!(json(2, &up)["event_action"], "resolve");
        assert_eq!(json(2, &up)["dedup_key"], "combd node 1 https://n1");
        assert_eq!(json(3, &up)["state"], "up");
    }

    /// A webhook really gets the POST, and an answer that isn't 2xx is a failure.
    #[test]
    fn a_webhook_gets_it() {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook", l.local_addr().unwrap());
        let t = std::thread::spawn(move || {
            let mut got = vec![];
            for status in ["200 OK", "500 No"] {
                let (mut s, _) = l.accept().unwrap();
                let mut buf = vec![];
                let mut b = [0u8; 4096];
                // The head, then as much body as its content-length says.
                loop {
                    let n = s.read(&mut b).unwrap();
                    buf.extend_from_slice(&b[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len: usize = head.lines().find_map(|l| l.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                        if body.len() >= len {
                            got.push(text);
                            break;
                        }
                    }
                }
                s.write_all(format!("HTTP/1.1 {status}\r\ncontent-length: 2\r\nconnection: close\r\n\r\nno").as_bytes()).unwrap();
            }
            got
        });
        let a = Alert { key: "k".into(), up: false, subject: "s".into(), text: "t".into() };
        let sinks = [Sink::Webhook(url)];
        assert!(tell(&sinks, &a));
        assert!(!tell(&sinks, &a), "a 500 isn't told");
        let got = t.join().unwrap();
        assert!(got[0].starts_with("POST /hook HTTP/1.1\r\n") && got[0].contains("content-type: application/json\r\n") && got[0].ends_with(r#""text":"t"}"#), "{}", got[0]);
    }

    #[test]
    fn how_long() {
        assert_eq!([lasted(20), lasted(170), lasted(3600), lasted(7300), lasted(11 * 86_400 + 5000)], ["1 minute", "2 minutes", "60 minutes", "2 hours", "11 days"]);
    }
}
