//! The four ways of measuring, and what each one's program says.
//!
//! Every program is run as it ships and read as text. None of them streams JSON:
//! speedtest-go's `--jsonl` is a single line at the very end, and neither list
//! command honours `--json` at all. Their plain output does stream, one line per
//! phase as it finishes, and cfspeedtest and iperf3 print a running figure as they
//! go, which is what the live value on the screen is. So a line is turned into an
//! [`Event`] here, and nothing about the screen is.
//!
//! The samples under `fixtures/` are what each program printed on a Flipper, with
//! the address and the provider swapped for documentation values.

/// Where a server is, as the picker lists it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Server {
    /// What the program is given to choose it: an id, or iperf3's host.
    pub id: String,
    pub name: String,
    /// A short aside drawn at the row's other edge: the latency speedtest-go
    /// measured to it while listing.
    pub note: String,
    /// Where it is and whose it is, for a list that is narrowed by place:
    /// LibreSpeed's. Empty where a program does not say.
    pub country: String,
    pub city: String,
    pub sponsor: String,
    /// A public iperf3 server's port, or the `first-last` range it answers on.
    pub ports: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dir {
    Down,
    Up,
}

/// One thing a program said that the screen cares about.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Which server the test actually ran against.
    Server(String),
    Ping {
        ms: f64,
        jitter: Option<f64>,
    },
    /// A figure from the middle of a phase, in Mbit/s.
    Live(Dir, f64),
    /// A phase's result, in Mbit/s.
    Done(Dir, f64),
    /// The program gave up, in its own words.
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Service {
    Ookla,
    LibreSpeed,
    Cloudflare,
    Iperf,
}

pub const SERVICES: [Service; 4] =
    [Service::Ookla, Service::LibreSpeed, Service::Cloudflare, Service::Iperf];

/// How long each iperf3 direction runs, in seconds: its own default.
const IPERF_SECONDS: &str = "10";

impl Service {
    pub fn name(self) -> &'static str {
        match self {
            Service::Ookla => "Ookla",
            Service::LibreSpeed => "LibreSpeed",
            Service::Cloudflare => "Cloudflare",
            Service::Iperf => "iperf3",
        }
    }

    /// The word the choice is saved under.
    pub fn key(self) -> &'static str {
        match self {
            Service::Ookla => "ookla",
            Service::LibreSpeed => "librespeed",
            Service::Cloudflare => "cloudflare",
            Service::Iperf => "iperf3",
        }
    }

    pub fn program(self) -> &'static str {
        match self {
            Service::Ookla => "speedtest-go",
            Service::LibreSpeed => "librespeed-cli",
            Service::Cloudflare => "cfspeedtest",
            Service::Iperf => "iperf3",
        }
    }

    /// Whether there is a list to pick from. Cloudflare is anycast, so the nearest
    /// of its sites is the only one it will answer from.
    pub fn listed(self) -> bool {
        matches!(self, Service::Ookla | Service::LibreSpeed)
    }

    /// Whether it reports latency. iperf3 measures throughput and nothing else.
    pub fn pings(self) -> bool {
        self != Service::Iperf
    }

    /// The list command. `near` is a place Ookla lists the servers near instead
    /// of near the user.
    pub fn list_args(self, near: Option<&Place>) -> Vec<String> {
        let mut argv = vec![self.program().into(), "--list".into()];
        if let (Service::Ookla, Some(place)) = (self, near) {
            argv.push(place.location());
        }
        argv
    }

    /// The commands a test is, in order: iperf3 measures one direction per run.
    /// `servers` is what the program may choose from, and empty leaves the choice
    /// to it. librespeed-cli takes several and tests against the one that answers
    /// first, which is what Auto inside a chosen country or city is; the others
    /// take one.
    ///
    /// Ookla's `near` goes on the test as well as the list: with no server given,
    /// speedtest-go picks the nearest one to wherever it believes it is, and that
    /// is the user unless it is told the place.
    pub fn runs(self, servers: &[&str], near: Option<&Place>) -> Vec<Vec<String>> {
        let program = self.program().to_string();
        let first = servers.first().copied();
        match self {
            Service::Ookla => {
                let mut argv = vec![program];
                if let Some(id) = first {
                    argv.extend(["-s".to_string(), id.to_string()]);
                }
                if let Some(place) = near {
                    argv.push(place.location());
                }
                vec![argv]
            }
            Service::LibreSpeed => {
                let mut argv = vec![program];
                for id in servers {
                    argv.extend(["--server".to_string(), id.to_string()]);
                }
                vec![argv]
            }
            Service::Cloudflare => vec![vec![program]],
            Service::Iperf => {
                let Some(host) = first else {
                    return Vec::new();
                };
                let (host, port) = split_port(host);
                let mut base = vec![
                    program,
                    "-c".into(),
                    host.into(),
                    "-t".into(),
                    IPERF_SECONDS.into(),
                    "--forceflush".into(),
                ];
                if let Some(port) = port {
                    base.extend(["-p".to_string(), port.to_string()]);
                }
                let mut down = base.clone();
                down.push("-R".into());
                vec![down, base]
            }
        }
    }

    pub fn parse_list(self, text: &str) -> Vec<Server> {
        match self {
            Service::Ookla => text.lines().filter_map(ookla_server).collect(),
            Service::LibreSpeed => {
                let mut servers: Vec<Server> = text.lines().filter_map(librespeed_server).collect();
                servers.sort_by(|a, b| a.name.cmp(&b.name));
                servers
            }
            _ => Vec::new(),
        }
    }

    /// One line of a test. `run` is which of [`Service::runs`] it came from.
    pub fn parse(self, line: &str, run: usize) -> Option<Event> {
        let line = line.trim();
        match self {
            Service::Ookla => ookla(line),
            Service::LibreSpeed => librespeed(line),
            Service::Cloudflare => cloudflare(line),
            Service::Iperf => iperf(line, if run == 0 { Dir::Down } else { Dir::Up }),
        }
    }
}

/// A city Ookla can be asked about, out of `cities.tsv`.
///
/// speedtest-go knows 26 cities by name, one for most countries, which left the
/// United Kingdom with London and Serbia with nothing. It takes any place as
/// coordinates, so the app keeps its own table, made from GeoNames by
/// `tools/tables.py`, and passes the one chosen.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub country: String,
    pub city: String,
    pub lat: f64,
    pub lon: f64,
    pub capital: bool,
}

impl Place {
    /// What speedtest-go takes for it.
    pub fn location(&self) -> String {
        format!("--location={:.4},{:.4}", self.lat, self.lon)
    }
}

/// The table, by country and then by city.
pub fn places() -> Vec<Place> {
    include_str!("../cities.tsv")
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let [_, country, city, lat, lon, capital] = fields.as_slice() else {
                return None;
            };
            Some(Place {
                country: country.to_string(),
                city: city.to_string(),
                lat: lat.parse().ok()?,
                lon: lon.parse().ok()?,
                capital: *capital == "1",
            })
        })
        .collect()
}

/// The public iperf3 servers, out of `iperf3-servers.tsv`: the list at
/// iperf3serverlist.net, cut down by `tools/tables.py`.
pub fn iperf3_servers() -> Vec<Server> {
    include_str!("../iperf3-servers.tsv")
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let [country, city, host, ports, provider] = fields.as_slice() else {
                return None;
            };
            Some(Server {
                id: host.to_string(),
                name: format!("{country}, {city} ({provider})"),
                country: country.to_string(),
                city: city.to_string(),
                sponsor: provider.to_string(),
                ports: ports.to_string(),
                ..Default::default()
            })
        })
        .collect()
}

/// How many ports of a range are tried before a server is given up on.
const PORTS_TRIED: usize = 4;

/// The `host:port`s to try a public server on, in order. A server that takes a
/// range runs one test per port, so a busy port is the usual answer and the next
/// one is the remedy; `from` spreads where in the range the tries start, so two
/// devices do not queue on the same port.
pub fn targets(server: &Server, from: usize) -> Vec<String> {
    let range =
        server.ports.split_once('-').unwrap_or((server.ports.as_str(), server.ports.as_str()));
    let (Ok(first), Ok(last)) = (range.0.parse::<u16>(), range.1.parse::<u16>()) else {
        return vec![server.id.clone()];
    };
    let count = usize::from(last.saturating_sub(first)) + 1;
    (0..count.min(PORTS_TRIED))
        .map(|i| format!("{}:{}", server.id, usize::from(first) + (from + i) % count))
        .collect()
}

/// `host:port`, for a server on a port other than iperf3's own. A string with more
/// than one colon is an IPv6 address and is taken whole.
fn split_port(host: &str) -> (&str, Option<&str>) {
    match host.split_once(':') {
        Some((name, port)) if !port.contains(':') && port.parse::<u16>().is_ok() => {
            (name, Some(port))
        }
        _ => (host, None),
    }
}

/// `[75213]     95.77km 2ms <tab>Belgrade (Serbia) by NETOPS `
fn ookla_server(line: &str) -> Option<Server> {
    let rest = line.trim().strip_prefix('[')?;
    let (id, rest) = rest.split_once(']')?;
    let id = id.trim();
    id.parse::<u32>().ok()?;
    let (figures, name) = rest.split_once('\t')?;
    let ping = figures.split_whitespace().nth(1).unwrap_or("");
    let note = ping
        .strip_suffix("ms")
        .and_then(|ms| ms.parse::<u32>().ok())
        .map(|ms| format!("{ms} ms"))
        .unwrap_or_default();
    let note = if ping == "Timeout" { "timeout".into() } else { note };
    Some(Server { id: id.into(), name: ookla_name(name.trim()), note, ..Default::default() })
}

/// `London (United Kingdom) by Octopus Telecom` as `Octopus Telecom, London`: the
/// provider first, because a city's servers all start with the same place and a row
/// is cut from the end. The country goes; the city is enough to tell towns apart.
fn ookla_name(name: &str) -> String {
    let Some((place, sponsor)) = name.split_once(" by ") else {
        return name.to_string();
    };
    let city = place.split(" (").next().unwrap_or(place).trim();
    format!("{}, {city}", sponsor.trim())
}

/// `106: Belgrade, Serbia (SOX) (https://...)  [Sponsor: ...]`
fn librespeed_server(line: &str) -> Option<Server> {
    let (id, rest) = line.split_once(": ")?;
    id.trim().parse::<u32>().ok()?;
    let name = rest.split(" (http").next()?.trim();
    if name.is_empty() {
        return None;
    }
    let (country, city, sponsor) = librespeed_place(name);
    Some(Server {
        id: id.trim().into(),
        name: librespeed_name(name),
        country,
        city,
        sponsor,
        ..Default::default()
    })
}

/// What the list calls one country by two names. Its entries are written by hand
/// by whoever runs each server, and two of them name the United States differently
/// and one names a state instead.
const COUNTRY_NAMES: &[(&str, &str)] = &[("USA", "United States"), ("Michigan", "United States")];

/// `Novi Sad, Vojvodina, Serbia (E-CAPS.net)` as its country, city and sponsor. A
/// region between the two goes. A number the list puts after the country to tell a
/// city's two servers apart, `Los Angeles, United States (1) (Clouvider)`, belongs
/// to the server and moves to its sponsor.
fn librespeed_place(name: &str) -> (String, String, String) {
    let (place, sponsor) = match name.rsplit_once(" (") {
        Some((place, sponsor)) if sponsor.ends_with(')') => (place, sponsor.trim_end_matches(')')),
        _ => (name, ""),
    };
    let (place, number) = match place.rsplit_once(" (") {
        Some((place, n)) if n.trim_end_matches(')').parse::<u32>().is_ok() => {
            (place, n.trim_end_matches(')'))
        }
        _ => (place, ""),
    };
    let parts: Vec<&str> = place.split(", ").map(str::trim).collect();
    let (city, country) = match parts.as_slice() {
        [city, .., country] => (*city, *country),
        _ => (place, ""),
    };
    let country = COUNTRY_NAMES.iter().find(|(n, _)| *n == country).map_or(country, |(_, to)| to);
    let sponsor =
        if number.is_empty() { sponsor.to_string() } else { format!("{sponsor} {number}") };
    (country.to_string(), city.to_string(), sponsor)
}

/// The same as `Serbia, Novi Sad (E-CAPS.net)`: the country first, so a list
/// sorted by name keeps each country's servers together.
fn librespeed_name(name: &str) -> String {
    let (country, city, sponsor) = librespeed_place(name);
    let place = if country.is_empty() { city } else { format!("{country}, {city}") };
    if sponsor.is_empty() {
        place
    } else {
        format!("{place} ({sponsor})")
    }
}

fn ookla(line: &str) -> Option<Event> {
    if let Some(rest) = line.strip_prefix("Test Server: ") {
        // `[75213] 0.90km Belgrade by NETOPS`: the distance goes, and the rest reads
        // the way the list does.
        let rest = rest.split_once("] ").map_or(rest, |(_, r)| r);
        let name = rest.split_once(' ').map_or(rest, |(_, r)| r);
        return Some(Event::Server(ookla_name(name)));
    }
    if let Some(rest) = line.strip_prefix("Latency: ") {
        let ms = duration_ms(rest.split_whitespace().next()?)?;
        let jitter = field(rest, "Jitter:").and_then(duration_ms);
        return Some(Event::Ping { ms, jitter });
    }
    if let Some(rest) = line.strip_prefix("Download: ") {
        return rate(rest).map(|r| Event::Done(Dir::Down, r));
    }
    if let Some(rest) = line.strip_prefix("Upload: ") {
        return rate(rest).map(|r| Event::Done(Dir::Up, r));
    }
    // `Fatal: retrieving user information, err: XML syntax error ...`: what it was
    // doing, which is the part that means something on a panel.
    if let Some(rest) = line.strip_prefix("Fatal: ") {
        let what = rest.split(", err:").next().unwrap_or(rest);
        return Some(Event::Failed(format!("Failed {what}")));
    }
    None
}

fn librespeed(line: &str) -> Option<Event> {
    if let Some(rest) = line.strip_prefix("Selected server: ") {
        return Some(Event::Server(librespeed_name(
            rest.split(" [").next().unwrap_or(rest).trim(),
        )));
    }
    if let Some(rest) = line.strip_prefix("Ping: ") {
        let ms = rest.split_whitespace().next()?.parse().ok()?;
        let jitter = field(rest, "Jitter:").and_then(|j| j.parse().ok());
        return Some(Event::Ping { ms, jitter });
    }
    if let Some(rest) = line.strip_prefix("Download rate:") {
        return rate(rest).map(|r| Event::Done(Dir::Down, r));
    }
    if let Some(rest) = line.strip_prefix("Upload rate:") {
        return rate(rest).map(|r| Event::Done(Dir::Up, r));
    }
    None
}

fn cloudflare(line: &str) -> Option<Event> {
    if let Some(colo) = line.strip_prefix("Colo: ") {
        return Some(Event::Server(format!("Cloudflare {}", colo.trim())));
    }
    if let Some(rest) = line.strip_prefix("Avg GET request latency ") {
        let ms = rest.split_whitespace().next()?.parse().ok()?;
        return Some(Event::Ping { ms, jitter: None });
    }
    let dir = if line.starts_with("Download") {
        Dir::Down
    } else if line.starts_with("Upload") {
        Dir::Up
    } else {
        return None;
    };
    // The summary, one line per payload size, smallest first: the last is the
    // largest payload, the one least weighed down by each request's own overhead.
    if let Some(avg) = field(line, "avg") {
        return avg.parse().ok().map(|r| Event::Done(dir, r));
    }
    // `Download 10MB   [===---]  419.16 mbit/s | 10MB in 190ms`, as it goes.
    let (_, after) = line.split_once(']')?;
    rate(after).map(|r| Event::Live(dir, r))
}

fn iperf(line: &str, dir: Dir) -> Option<Event> {
    if let Some(rest) = line.strip_prefix("iperf3: error - ") {
        // `unable to connect to server - server may have stopped running ...:
        // Connection refused`: the reason is the last clause.
        let why = rest.rsplit(": ").next().unwrap_or(rest).trim();
        return Some(Event::Failed(capitalise(why)));
    }
    if let Some(rest) = line.strip_prefix("Connecting to host ") {
        return Some(Event::Server(rest.split(',').next().unwrap_or(rest).into()));
    }
    if !line.starts_with('[') || line.contains("sender") {
        return None;
    }
    let mbps = iperf_rate(line)?;
    Some(if line.contains("receiver") { Event::Done(dir, mbps) } else { Event::Live(dir, mbps) })
}

/// `18.2 Gbits/sec`, wherever it sits in the line.
fn iperf_rate(line: &str) -> Option<f64> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let at = words.iter().position(|w| w.ends_with("bits/sec"))?;
    let value: f64 = words.get(at.checked_sub(1)?)?.parse().ok()?;
    let scale = match words[at].chars().next()? {
        'G' => 1000.0,
        'M' => 1.0,
        'K' | 'k' => 0.001,
        'b' => 0.000_001,
        _ => return None,
    };
    Some(value * scale)
}

/// The first `<number> <unit>` in `text`, in Mbit/s: `148.74 Mbps`, `52.11 Mbps`,
/// `419.16 mbit/s`.
fn rate(text: &str) -> Option<f64> {
    let words: Vec<&str> = text.split_whitespace().collect();
    words.windows(2).find_map(|pair| {
        let value: f64 = pair[0].parse().ok()?;
        let scale = match pair[1].to_ascii_lowercase().as_str() {
            "gbps" | "gbit/s" => 1000.0,
            "mbps" | "mbit/s" => 1.0,
            "kbps" | "kbit/s" => 0.001,
            _ => return None,
        };
        Some(value * scale)
    })
}

/// The word after `label` in `text`.
fn field<'a>(text: &'a str, label: &str) -> Option<&'a str> {
    let mut words = text.split_whitespace();
    words.position(|w| w == label)?;
    words.next()
}

/// A Go duration as speedtest-go prints one, in milliseconds: `2.91429ms`,
/// `76.573µs`.
fn duration_ms(text: &str) -> Option<f64> {
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (value, unit) = text.split_at(split);
    let value: f64 = value.parse().ok()?;
    let scale = match unit {
        "ns" => 0.000_001,
        "µs" | "us" => 0.001,
        "ms" => 1.0,
        "s" => 1000.0,
        _ => return None,
    };
    Some(value * scale)
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a test's output adds up to, the way the app reads it: a line at a
    /// time, with `\r` ending a line as much as `\n` does.
    fn events(service: Service, text: &str, run: usize) -> Vec<Event> {
        text.split(['\r', '\n']).filter_map(|line| service.parse(line, run)).collect()
    }

    fn done(events: &[Event], dir: Dir) -> Option<f64> {
        events.iter().rev().find_map(|e| match e {
            Event::Done(d, r) if *d == dir => Some(*r),
            _ => None,
        })
    }

    fn place(country: &str, city: &str) -> Place {
        places().into_iter().find(|p| p.country == country && p.city == city).expect(city)
    }

    #[test]
    fn the_table_has_every_country_and_its_cities() {
        let places = places();
        let countries: std::collections::BTreeSet<&str> =
            places.iter().map(|p| p.country.as_str()).collect();
        assert!(countries.len() > 200, "{}", countries.len());
        for (country, city) in [
            ("Serbia", "Belgrade"),
            ("Serbia", "Novi Sad"),
            ("United Kingdom", "Manchester"),
            ("Brazil", "Brasilia"),
        ] {
            assert!(places.iter().any(|p| p.country == country && p.city == city), "{city}");
        }
        assert!(place("Serbia", "Belgrade").capital);
        // A district is not a city of its own.
        assert!(!places.iter().any(|p| p.city == "Zemun" || p.city == "Brooklyn"));
        assert!(
            places.iter().all(|p| p.city.is_ascii() && p.country.is_ascii()),
            "the fonts are ASCII"
        );
        assert!(places
            .windows(2)
            .all(|w| (&w[0].country, &w[0].city) < (&w[1].country, &w[1].city)));
    }

    #[test]
    fn a_place_only_moves_the_ookla_list() {
        let manchester = place("United Kingdom", "Manchester");
        assert_eq!(
            Service::Ookla.list_args(Some(&manchester)).join(" "),
            "speedtest-go --list --location=53.4809,-2.2374"
        );
        assert_eq!(Service::Ookla.list_args(None).join(" "), "speedtest-go --list");
        assert_eq!(
            Service::LibreSpeed.list_args(Some(&manchester)).join(" "),
            "librespeed-cli --list"
        );
    }

    #[test]
    fn ookla_lists_twenty_servers_with_their_latency() {
        let servers = Service::Ookla.parse_list(include_str!("../fixtures/ookla-list.txt"));
        assert_eq!(servers.len(), 20);
        assert_eq!(
            servers[1],
            Server {
                id: "75213".into(),
                name: "NETOPS, Belgrade".into(),
                note: "2 ms".into(),
                ..Default::default()
            }
        );
        // A four-digit id is padded inside its brackets.
        assert!(servers.iter().any(|s| s.id == "3600" && s.name == "E-CAPS.net, Novi Sad"));
    }

    #[test]
    fn ookla_reports_each_phase_as_it_ends() {
        let got = events(Service::Ookla, include_str!("../fixtures/ookla-run.txt"), 0);
        assert_eq!(got[0], Event::Server("NETOPS, Belgrade".into()));
        let Event::Ping { ms, jitter } = got[1] else { panic!("{:?}", got[1]) };
        assert!((ms - 2.91429).abs() < 1e-9);
        assert!((jitter.unwrap() - 0.076573).abs() < 1e-9, "microseconds, in ms");
        assert_eq!(done(&got, Dir::Down), Some(148.74));
        assert_eq!(done(&got, Dir::Up), Some(366.93));
    }

    /// Rows from the list near London, where every name starts with the same place.
    #[test]
    fn an_ookla_server_reads_provider_first() {
        let list =
            "[64515]   1767.89km 62ms \tLondon (United Kingdom) by Airband Community Internet \n\
                    [76436]   1767.26km Timeout \tHarlow (United Kingdom) by Virgin Media O2 \n\
                    [ 6115]   1772.24km 71ms \tLondon (United Kingdom) by Cybersmart Pty Ltd \n";
        let servers = Service::Ookla.parse_list(list);
        assert_eq!(servers[0].name, "Airband Community Internet, London");
        assert_eq!(servers[0].note, "62 ms");
        assert_eq!(servers[1].name, "Virgin Media O2, Harlow");
        assert_eq!(servers[1].note, "timeout");
        assert_eq!(servers[2].id, "6115");
        assert_eq!(ookla_name("Somewhere"), "Somewhere", "a name without a sponsor stays");
    }

    #[test]
    fn ookla_says_what_it_was_doing_when_it_gave_up() {
        let got = events(Service::Ookla, include_str!("../fixtures/ookla-fatal.txt"), 0);
        assert_eq!(got, vec![Event::Failed("Failed retrieving user information".into())]);
    }

    #[test]
    fn librespeed_lists_by_name_without_the_url() {
        let servers =
            Service::LibreSpeed.parse_list(include_str!("../fixtures/librespeed-list.txt"));
        assert!(servers.len() > 20, "{}", servers.len());
        let sox = servers.iter().find(|s| s.id == "106").expect("server 106");
        assert_eq!(sox.name, "Serbia, Belgrade (SOX)");
        let novi_sad = servers.iter().find(|s| s.name.contains("Novi Sad")).expect("Novi Sad");
        assert_eq!(novi_sad.name, "Serbia, Novi Sad (E-CAPS.net)", "the region goes");
        assert!(servers.windows(2).all(|w| w[0].name <= w[1].name), "each country together");
        assert_eq!(
            (sox.country.as_str(), sox.city.as_str(), sox.sponsor.as_str()),
            ("Serbia", "Belgrade", "SOX")
        );
        // The list's own spellings of one country, and a city's numbered pair.
        let la: Vec<&Server> = servers.iter().filter(|s| s.city == "Los Angeles").collect();
        assert!(la.iter().all(|s| s.country == "United States"), "{la:?}");
        assert!(la.iter().any(|s| s.sponsor == "Clouvider 1"), "{la:?}");
        let rapids = servers.iter().find(|s| s.city == "Grand Rapids").expect("Grand Rapids");
        assert_eq!(rapids.country, "United States");
        assert!(servers.iter().all(|s| !s.name.contains("http")));
    }

    // 3.14 is the latency the device measured, not an attempt at pi.
    #[allow(clippy::approx_constant)]
    #[test]
    fn librespeed_reports_each_phase_as_it_ends() {
        let got = events(Service::LibreSpeed, include_str!("../fixtures/librespeed-run.txt"), 0);
        assert_eq!(got[0], Event::Server("Serbia, Belgrade (SOX)".into()));
        assert_eq!(got[1], Event::Ping { ms: 3.14, jitter: Some(0.12) });
        assert_eq!(done(&got, Dir::Down), Some(52.11));
        assert_eq!(done(&got, Dir::Up), Some(361.83));
    }

    #[test]
    fn cloudflare_goes_live_and_ends_on_the_largest_payload() {
        let got = events(Service::Cloudflare, include_str!("../fixtures/cloudflare-run.txt"), 0);
        assert_eq!(got[0], Event::Server("Cloudflare BEG".into()));
        assert_eq!(got[1], Event::Ping { ms: 31.53, jitter: None });
        assert_eq!(got[2], Event::Live(Dir::Down, 8.44));
        assert!(got.iter().filter(|e| matches!(e, Event::Live(Dir::Up, _))).count() > 10);
        assert_eq!(done(&got, Dir::Down), Some(389.01), "the 25MB average");
        assert_eq!(done(&got, Dir::Up), Some(330.21));
    }

    #[test]
    fn iperf_goes_live_and_ends_on_the_receiver() {
        let down = events(Service::Iperf, include_str!("../fixtures/iperf-down.txt"), 0);
        assert_eq!(down[0], Event::Server("127.0.0.1".into()));
        assert_eq!(down[1], Event::Live(Dir::Down, 19200.0));
        assert_eq!(done(&down, Dir::Down), Some(19600.0));
        let up = events(Service::Iperf, include_str!("../fixtures/iperf-up.txt"), 1);
        assert_eq!(up[1], Event::Live(Dir::Up, 18200.0));
        assert_eq!(done(&up, Dir::Up), Some(18700.0));
    }

    #[test]
    fn iperf_gives_the_reason_it_could_not_connect() {
        let got = events(Service::Iperf, include_str!("../fixtures/iperf-refused.txt"), 0);
        assert_eq!(got, vec![Event::Failed("Connection refused".into())]);
    }

    #[test]
    fn the_public_iperf_servers_read_with_their_place() {
        let servers = iperf3_servers();
        assert!(servers.len() > 100, "{}", servers.len());
        let sox = servers.iter().find(|s| s.id == "speedtest1.sox.rs").expect("SOX");
        assert_eq!(
            (sox.country.as_str(), sox.city.as_str(), sox.sponsor.as_str()),
            ("Serbia", "Belgrade", "SOX")
        );
        assert_eq!(sox.ports, "9206-9240");
        assert!(servers.iter().all(|s| s.name.is_ascii()), "the fonts are ASCII");
    }

    #[test]
    fn a_port_range_is_tried_a_few_ports_at_a_time() {
        let sox = Server {
            id: "speedtest1.sox.rs".into(),
            ports: "9206-9240".into(),
            ..Default::default()
        };
        assert_eq!(
            targets(&sox, 0),
            [
                "speedtest1.sox.rs:9206",
                "speedtest1.sox.rs:9207",
                "speedtest1.sox.rs:9208",
                "speedtest1.sox.rs:9209"
            ]
        );
        // From further in, and round the end of the range.
        assert_eq!(targets(&sox, 34)[..2], ["speedtest1.sox.rs:9240", "speedtest1.sox.rs:9206"]);
        let one = Server { id: "185.59.221.51".into(), ports: "5201".into(), ..Default::default() };
        assert_eq!(targets(&one, 7), ["185.59.221.51:5201"]);
        // A host typed in carries its own port, or none.
        let typed = Server { id: "nas.lan:5202".into(), ..Default::default() };
        assert_eq!(targets(&typed, 3), ["nas.lan:5202"]);
    }

    #[test]
    fn iperf_runs_download_then_upload_and_takes_a_port() {
        let runs = Service::Iperf.runs(&["nas.lan:5202"], None);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].join(" "), "iperf3 -c nas.lan -t 10 --forceflush -p 5202 -R");
        assert_eq!(runs[1].join(" "), "iperf3 -c nas.lan -t 10 --forceflush -p 5202");
        assert_eq!(Service::Iperf.runs(&["fe80::1"], None)[1][2], "fe80::1");
        assert!(Service::Iperf.runs(&[], None).is_empty(), "iperf3 has no server of its own");
    }

    #[test]
    fn a_chosen_server_is_passed_and_none_lets_the_program_choose() {
        // Auto inside a place: librespeed-cli chooses among that place's servers.
        assert_eq!(
            Service::LibreSpeed.runs(&["50", "105"], None)[0].join(" "),
            "librespeed-cli --server 50 --server 105"
        );
        assert_eq!(
            Service::Ookla.runs(&["75213"], None),
            vec![vec!["speedtest-go", "-s", "75213"]]
        );
        assert_eq!(Service::Ookla.runs(&[], None), vec![vec!["speedtest-go"]]);
        // The nearest server to the place chosen, not to the user.
        let manchester = place("United Kingdom", "Manchester");
        assert_eq!(
            Service::Ookla.runs(&[], Some(&manchester))[0].join(" "),
            "speedtest-go --location=53.4809,-2.2374"
        );
        assert_eq!(
            Service::Ookla.runs(&["48475"], Some(&manchester))[0].join(" "),
            "speedtest-go -s 48475 --location=53.4809,-2.2374"
        );
        assert_eq!(Service::LibreSpeed.runs(&[], Some(&manchester)), vec![vec!["librespeed-cli"]]);
        assert_eq!(
            Service::LibreSpeed.runs(&["106"], None)[0].join(" "),
            "librespeed-cli --server 106"
        );
        assert_eq!(Service::Cloudflare.runs(&[], None), vec![vec!["cfspeedtest"]]);
    }
}
