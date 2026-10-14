//! Speed test, as a flipctl app: the link measured against a server the user picks.
//!
//! Four services, each measured by a program the bundle carries: speedtest-go on
//! Ookla's servers, librespeed-cli on LibreSpeed's, cfspeedtest on Cloudflare's, and
//! iperf3 on a host of the user's own. The app starts the program, reads what it
//! prints and puts it on the page; `service` is where a line becomes a number.
//!
//! Two pages. The settings are flipctl's own menu rows: the service, for Ookla the
//! city whose servers it lists, and the server, each a "< value >" that Left and
//! Right step and Ok opens in full, as a whole-screen list or, for an iperf3 host,
//! the keyboard. Start moves to the results, a detail page where a figure still
//! being measured says so, and where cfspeedtest's and iperf3's running values take
//! its place until the result arrives.

mod child;
mod service;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use flipctl_app::theme::{count, metric, radius};
use flipctl_app::{font, keyboard, Key, StatusSource};
use service::{Dir, Event, Place, Server, Service, SERVICES};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

slint::include_modules!();

/// How often the programs are read. Fast enough for cfspeedtest's running figure,
/// which changes with every request it makes.
const TICK: Duration = Duration::from_millis(200);

/// The iperf3 hosts kept, most recent first.
const KEPT_HOSTS: usize = 8;

/// How many of a place's public iperf3 servers Auto tries before it gives up.
const AUTO_SERVERS: usize = 8;

/// Where the choices are kept, in the working directory flipctl gives the app.
const STORE: &str = "speedtest.conf";

/// What a figure says while it is being measured, and when there is none.
const MEASURING: &str = "...";
const NONE: &str = "-";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Row {
    Service,
    /// A country, then a city in it: Ookla's out of the city table, whose servers
    /// it is asked to list, and LibreSpeed's out of its own list, which they narrow.
    Country,
    Town,
    Server,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Page {
    Settings,
    Results,
}

/// What a whole-screen list is choosing.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Listing {
    Country,
    Town,
    Server,
}

/// One row of a whole-screen list.
#[derive(Clone, Debug, PartialEq)]
enum Pick {
    /// Let the program choose: the nearest server, or the user's own location.
    Auto,
    Server(Server),
    /// Type an iperf3 host.
    Enter,
    /// A country or a city.
    Name(String),
    /// A line that is not a choice: the list on its way, or why it never came.
    Note(String),
}

struct Picker {
    what: Listing,
    items: Vec<Pick>,
    selected: usize,
    scroll: usize,
}

/// A list command's answer: the servers, or why there are none.
type ListAnswer = Result<Vec<Server>, String>;

#[derive(Default)]
struct Results {
    server: Option<String>,
    ping: Option<f64>,
    jitter: Option<f64>,
    down: Option<f64>,
    up: Option<f64>,
    /// A running figure from the phase under way.
    live: Option<(Dir, f64)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Ping,
    Down,
    Up,
}

struct Test {
    runs: Vec<Vec<String>>,
    at: usize,
    /// iperf3's `host:port`s still to try, and what each one is called on the
    /// results page. A public server answers one test per port, so a busy port, or
    /// a server that is down, moves the test on to the next rather than ending it.
    targets: Vec<(String, String)>,
    target: usize,
    run: child::Run,
    /// Why it failed, in the program's words, once it has said.
    failed: Option<String>,
    /// The last line no parser wanted, for a program that ends badly without
    /// saying why in a way this app knows.
    last: Option<String>,
}

struct App {
    service: usize,
    page: Page,
    /// The settings row the selector is on.
    row: usize,
    /// Which chevron is held: 0 none, 1 left, 2 right.
    arrow: i32,
    /// The server chosen per service. None leaves it to the program.
    chosen: Vec<Option<Server>>,
    /// The country and city chosen per service. None is Auto: for Ookla the
    /// user's own location, for LibreSpeed every server.
    country: Vec<Option<String>>,
    town: Vec<Option<String>>,
    /// The cities Ookla can be asked about.
    places: Vec<Place>,
    hosts: Vec<String>,
    /// Each service's list, once fetched. Ookla's is the current city's.
    lists: Vec<Option<Vec<Server>>>,
    /// The list on its way, and which service's it is.
    loading: Option<(usize, Receiver<ListAnswer>)>,
    /// Why the last list never came.
    list_failed: Option<String>,
    results: Results,
    test: Option<Test>,
    picker: Option<Picker>,
    kb: Option<keyboard::TextInput>,
    failed: Option<String>,
    store: Option<PathBuf>,
}

impl App {
    fn new(store: Option<PathBuf>) -> Self {
        let mut app = Self {
            service: 0,
            page: Page::Settings,
            row: 0,
            arrow: 0,
            chosen: vec![None; SERVICES.len()],
            country: vec![None; SERVICES.len()],
            town: vec![None; SERVICES.len()],
            places: service::places(),
            hosts: Vec::new(),
            // iperf3's public servers are the app's own table, so its list is here
            // from the start.
            lists: vec![None, None, None, Some(service::iperf3_servers())],
            loading: None,
            list_failed: None,
            results: Results::default(),
            test: None,
            picker: None,
            kb: None,
            failed: None,
            store,
        };
        app.load();
        app
    }

    fn current(&self) -> Service {
        SERVICES[self.service]
    }

    // The choices, as `key=value` lines: the service, each service's country and
    // city, each service's server as its id and name across a tab, and the iperf3
    // hosts in order.
    fn load(&mut self) {
        let Some(text) = self.store.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return;
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (id, name) = value.split_once('\t').unwrap_or((value, value));
            if key == "service" {
                if let Some(at) = SERVICES.iter().position(|s| s.key() == value) {
                    self.service = at;
                }
            } else if key == "host" {
                self.hosts.push(value.to_string());
            } else if let Some((at, place)) = SERVICES.iter().enumerate().find_map(|(at, s)| {
                key.strip_prefix(s.key()).and_then(|k| k.strip_prefix('-')).map(|k| (at, k))
            }) {
                match place {
                    "country" => self.country[at] = Some(value.to_string()),
                    "city" => self.town[at] = Some(value.to_string()),
                    _ => {}
                }
            } else if let Some(at) = SERVICES.iter().position(|s| s.key() == key) {
                self.chosen[at] =
                    Some(Server { id: id.into(), name: name.into(), ..Default::default() });
            }
        }
    }

    fn save(&self) {
        let Some(path) = self.store.as_ref() else {
            return;
        };
        let mut text = format!("service={}\n", self.current().key());
        for (at, service) in SERVICES.iter().enumerate() {
            if let Some(country) = &self.country[at] {
                text += &format!("{}-country={country}\n", service.key());
            }
            if let Some(town) = &self.town[at] {
                text += &format!("{}-city={town}\n", service.key());
            }
        }
        for (service, chosen) in SERVICES.iter().zip(&self.chosen) {
            if let Some(server) = chosen {
                text += &format!("{}={}\t{}\n", service.key(), server.id, server.name);
            }
        }
        for host in &self.hosts {
            text += &format!("host={host}\n");
        }
        let _ = std::fs::write(path, text);
    }

    /// The settings rows this service has. Ookla lists the servers near a place it
    /// is told; LibreSpeed and iperf3 list them all, so theirs are narrowed here.
    /// All by country and then by city. Cloudflare has nowhere to choose.
    fn rows(&self) -> Vec<Row> {
        match self.current() {
            Service::Cloudflare => vec![Row::Service, Row::Server],
            _ => vec![Row::Service, Row::Country, Row::Town, Row::Server],
        }
    }

    /// Whether the service's own list says where each server is, so the place rows
    /// narrow it rather than move it.
    fn narrows(&self) -> bool {
        matches!(self.current(), Service::LibreSpeed | Service::Iperf)
    }

    /// Whether a server is in the country and city chosen. A host the user typed
    /// is in no country and is offered wherever they are.
    fn in_place(&self, server: &Server) -> bool {
        server.country.is_empty()
            || (self.country[self.service].as_ref().is_none_or(|c| *c == server.country)
                && self.town[self.service].as_ref().is_none_or(|t| *t == server.city))
    }

    fn placed(&self) -> bool {
        self.country[self.service].is_some() || self.town[self.service].is_some()
    }

    /// Where Ookla is asked about: the city chosen, or the capital of the country
    /// chosen with its city on Auto. None is the user's own location.
    fn ookla_place(&self) -> Option<&Place> {
        let country = self.country[0].as_ref()?;
        let mut in_country = self.places.iter().filter(|p| p.country == *country);
        match &self.town[0] {
            Some(town) => in_country.find(|p| p.city == *town),
            None => {
                let all: Vec<&Place> = in_country.collect();
                all.iter().find(|p| p.capital).or(all.first()).copied()
            }
        }
    }

    /// The servers the server row chooses between: all of Ookla's list, or the
    /// others' inside the country and city chosen.
    fn listed_servers(&self) -> Vec<Server> {
        let list = self.lists[self.service].as_deref().unwrap_or_default();
        if self.narrows() {
            list.iter().filter(|s| self.in_place(s)).cloned().collect()
        } else {
            list.to_vec()
        }
    }

    /// The iperf3 hosts the user typed, as servers.
    fn own_hosts(&self) -> Vec<Server> {
        self.hosts
            .iter()
            .map(|h| Server {
                id: h.clone(),
                name: h.clone(),
                note: "yours".into(),
                ..Default::default()
            })
            .collect()
    }

    /// The countries to choose from, or the cities of the one chosen, sorted,
    /// once: Ookla's out of the city table, the others' out of their lists.
    fn place_names(&self, what: Listing) -> Vec<String> {
        let country = &self.country[self.service];
        let wanted =
            |c: &String| what == Listing::Country || country.as_ref().is_none_or(|w| w == c);
        let pairs: Vec<(&String, &String)> = match self.current() {
            Service::Ookla => self.places.iter().map(|p| (&p.country, &p.city)).collect(),
            _ => self.lists[self.service]
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|s| (&s.country, &s.city))
                .collect(),
        };
        let mut names: Vec<String> = pairs
            .into_iter()
            .filter(|(c, _)| wanted(c))
            .map(|(c, t)| if what == Listing::Country { c.clone() } else { t.clone() })
            .filter(|n| !n.is_empty())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn set_country(&mut self, country: Option<String>) {
        if country == self.country[self.service] {
            return;
        }
        self.country[self.service] = country;
        self.town[self.service] = None;
        self.moved();
    }

    fn set_town(&mut self, town: Option<String>) {
        if town == self.town[self.service] {
            return;
        }
        self.town[self.service] = town;
        self.moved();
    }

    /// After the place changed. Ookla's list was of the old place's servers, so it
    /// goes, and so does a server out of it. The others' lists are the same lists,
    /// and only a server outside the new place goes.
    fn moved(&mut self) {
        if self.current() == Service::Ookla {
            self.lists[0] = None;
            if self.loading.as_ref().is_some_and(|(at, _)| *at == 0) {
                self.loading = None;
            }
            self.chosen[0] = None;
            self.save();
        } else {
            self.let_go_outside_place();
        }
    }

    /// A server chosen before the place changed, if it is no longer in it. One
    /// the list does not know, a typed host or a list not fetched yet, stays.
    fn let_go_outside_place(&mut self) {
        if let Some(server) = self.chosen[self.service].clone() {
            let known = self.known(&server.id);
            if known.is_some_and(|s| !self.in_place(&s)) {
                self.chosen[self.service] = None;
            }
        }
        self.save();
    }

    /// The list's own record of a server, which knows its place and its ports.
    fn known(&self, id: &str) -> Option<Server> {
        self.lists[self.service].as_deref().unwrap_or_default().iter().find(|s| s.id == id).cloned()
    }

    /// How a server reads on its row and in the list: one out of a narrowed list
    /// loses what the rows above already say.
    fn server_text(&self, server: &Server) -> String {
        if !self.narrows() || server.sponsor.is_empty() {
            return server.name.clone();
        }
        match (&self.country[self.service], &self.town[self.service]) {
            (_, Some(_)) => server.sponsor.clone(),
            (Some(_), None) => format!("{} ({})", server.city, server.sponsor),
            (None, None) => server.name.clone(),
        }
    }

    fn running(&self) -> bool {
        self.test.is_some()
    }

    /// The figure being measured now, which is the first one still missing.
    fn phase(&self) -> Option<Phase> {
        self.test.as_ref()?;
        let r = &self.results;
        if self.current().pings() && r.ping.is_none() {
            Some(Phase::Ping)
        } else if r.down.is_none() {
            Some(Phase::Down)
        } else if r.up.is_none() {
            Some(Phase::Up)
        } else {
            None
        }
    }

    fn set_service(&mut self, index: usize) {
        self.service = index;
        self.row = self.row.min(self.rows().len() - 1);
        self.save();
    }

    /// What the server row can be set to, in order. Empty when there is nothing to
    /// choose between, which stops the chevrons cycling one thing into itself.
    fn server_options(&self) -> Vec<Option<Server>> {
        match self.current() {
            Service::Cloudflare => Vec::new(),
            Service::Iperf => {
                let auto = self.placed().then_some(None);
                let servers = self.listed_servers().into_iter().chain(self.own_hosts()).map(Some);
                auto.into_iter().chain(servers).collect()
            }
            _ => match &self.lists[self.service] {
                Some(_) => std::iter::once(None)
                    .chain(self.listed_servers().into_iter().map(Some))
                    .collect(),
                None => Vec::new(),
            },
        }
    }

    fn cycle(&mut self, row: Row, step: isize) {
        let next =
            |at: usize, total: usize| (at as isize + step).rem_euclid(total as isize) as usize;
        match row {
            Row::Service => self.set_service(next(self.service, SERVICES.len())),
            Row::Country | Row::Town => {
                if self.narrows() && self.lists[self.service].is_none() {
                    self.fetch_list();
                    return;
                }
                let what = if row == Row::Country { Listing::Country } else { Listing::Town };
                let options: Vec<Option<String>> = std::iter::once(None)
                    .chain(self.place_names(what).into_iter().map(Some))
                    .collect();
                let current = if row == Row::Country {
                    &self.country[self.service]
                } else {
                    &self.town[self.service]
                };
                let at = options.iter().position(|o| o == current).unwrap_or(0);
                let chosen = options[next(at, options.len())].clone();
                if row == Row::Country {
                    self.set_country(chosen);
                } else {
                    self.set_town(chosen);
                }
            }
            Row::Server => {
                let options = self.server_options();
                if options.len() < 2 {
                    // A listed service's list is fetched the first time it is
                    // wanted, and the chevrons work once it is here.
                    if self.current().listed() {
                        self.fetch_list();
                    }
                    return;
                }
                let chosen = self.chosen[self.service].as_ref().map(|s| &s.id);
                let at =
                    options.iter().position(|o| o.as_ref().map(|s| &s.id) == chosen).unwrap_or(0);
                self.choose(options[next(at, options.len())].clone());
            }
        }
    }

    /// Ok on a row: the whole of what it can be set to.
    fn open(&mut self, row: Row) {
        match row {
            Row::Service => self.cycle(Row::Service, 1),
            Row::Country => self.open_picker(Listing::Country),
            Row::Town => self.open_picker(Listing::Town),
            Row::Server => match self.current() {
                Service::Cloudflare => {}
                _ => self.open_picker(Listing::Server),
            },
        }
    }

    /// The commands the next test is, worked out from the rows, and for iperf3 the
    /// targets it tries in turn.
    fn test_runs(&mut self) -> (Vec<Vec<String>>, Vec<(String, String)>) {
        let service = self.current();
        if service == Service::Iperf {
            let targets = self.iperf_targets();
            let runs = targets.first().map_or_else(Vec::new, |(t, _)| service.runs(&[t], None));
            return (runs, targets);
        }
        let narrowed = service == Service::LibreSpeed && self.placed();
        if narrowed && self.lists[1].is_none() {
            // Auto inside a place needs that place's servers to choose from, and
            // the list has not been fetched this session. It is a second's wait.
            if let Ok(text) = child::output(&service.list_args(None)) {
                self.lists[1] = Some(service.parse_list(&text));
            }
        }
        let ids: Vec<String> = match &self.chosen[self.service] {
            Some(server) => vec![server.id.clone()],
            None if narrowed => self.listed_servers().into_iter().map(|s| s.id).collect(),
            None => Vec::new(),
        };
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        (service.runs(&ids, self.ookla_place()), Vec::new())
    }

    /// Where iperf3 goes: the server chosen on a few of its ports, or with Auto in
    /// a place, each of the place's servers on one port, until one answers.
    fn iperf_targets(&self) -> Vec<(String, String)> {
        // Where in a port range to start, different from one test to the next.
        let from = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos() as usize);
        match &self.chosen[self.service] {
            Some(server) => {
                let server = self.known(&server.id).unwrap_or_else(|| server.clone());
                let label = self.server_text(&server);
                service::targets(&server, from).into_iter().map(|t| (t, label.clone())).collect()
            }
            None if self.placed() => self
                .listed_servers()
                .iter()
                .filter(|s| !s.country.is_empty())
                .take(AUTO_SERVERS)
                .filter_map(|s| {
                    Some((service::targets(s, from).into_iter().next()?, self.server_text(s)))
                })
                .collect(),
            None => Vec::new(),
        }
    }

    fn start(&mut self) {
        let service = self.current();
        let (runs, targets) = self.test_runs();
        if runs.is_empty() {
            // Only iperf3 has nothing to fall back on: with no place and no server
            // there is nowhere to go, so the row that chooses one is where the test
            // starts.
            self.page = Page::Settings;
            self.row = self.rows().iter().position(|r| *r == Row::Server).unwrap_or(0);
            self.open(Row::Server);
            return;
        }
        self.results = Results::default();
        self.results.server = targets.first().map(|(_, label)| label.clone());
        match child::Run::start(&runs[0]) {
            Ok(run) => {
                self.test =
                    Some(Test { runs, at: 0, targets, target: 0, run, failed: None, last: None });
                self.page = Page::Results;
            }
            Err(e) => self.failed = Some(child::missing(service.program(), &e)),
        }
    }

    fn stop(&mut self) {
        self.test = None;
        self.results.live = None;
    }

    /// Read what the program has said. True when anything changed.
    fn poll(&mut self) -> bool {
        let service = self.current();
        let Some(test) = self.test.as_mut() else {
            return false;
        };
        let (lines, exited) = match test.run.poll() {
            child::Polled::Lines(lines) => (lines, None),
            child::Polled::Exited(lines, status) => (lines, Some(status)),
        };
        let changed = !lines.is_empty() || exited.is_some();
        let at = test.at;
        for line in lines {
            match service.parse(&line, at) {
                Some(Event::Failed(why)) => test.failed = Some(why),
                // iperf3 names the host it connects to, and the page already says
                // which server that is.
                Some(Event::Server(_)) if !test.targets.is_empty() => {}
                Some(event) => apply_event(&mut self.results, event),
                None => test.last = Some(line.trim().to_string()),
            }
        }
        let Some(status) = exited else {
            return changed;
        };
        let measured = match service {
            Service::Iperf if at == 0 => self.results.down.is_some(),
            _ => self.results.up.is_some(),
        };
        let failed = test.failed.is_some() || !status.success() || !measured;
        if failed && test.target + 1 < test.targets.len() {
            // The next port, or the next server: the same direction again there.
            test.target += 1;
            let (target, label) = test.targets[test.target].clone();
            test.runs = service.runs(&[&target], None);
            test.failed = None;
            test.last = None;
            self.results.live = None;
            match child::Run::start(&test.runs[at]) {
                Ok(run) => test.run = run,
                Err(e) => {
                    self.failed = Some(child::missing(service.program(), &e));
                    self.stop();
                    return true;
                }
            }
            self.results.server = Some(label);
            return true;
        }
        if failed {
            let why = test.failed.take().or(test.last.take());
            self.failed =
                Some(why.unwrap_or_else(|| format!("{} stopped early", service.program())));
            self.stop();
            return true;
        }
        test.at += 1;
        if test.at < test.runs.len() {
            match child::Run::start(&test.runs[test.at]) {
                Ok(run) => test.run = run,
                Err(e) => {
                    self.failed = Some(child::missing(service.program(), &e));
                    self.stop();
                }
            }
        } else {
            self.stop();
        }
        true
    }

    /// Ask the program for its list, off the thread that draws: speedtest-go pings
    /// every server it lists, which takes a few seconds.
    fn fetch_list(&mut self) {
        let service = self.current();
        if self.lists[self.service].is_some()
            || self.loading.as_ref().is_some_and(|(at, _)| *at == self.service)
        {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let argv = service.list_args(self.ookla_place());
        std::thread::spawn(move || {
            let _ = tx.send(child::output(&argv).map(|text| service.parse_list(&text)));
        });
        self.loading = Some((self.service, rx));
        self.list_failed = None;
    }

    /// The list, if the thread fetching it has come back. True when it has.
    fn poll_list(&mut self) -> bool {
        let Some((at, rx)) = self.loading.as_ref() else {
            return false;
        };
        let Ok(answer) = rx.try_recv() else {
            return false;
        };
        let at = *at;
        self.loading = None;
        match answer {
            Ok(list) => {
                // A server kept from an earlier session takes the list's own record
                // of it, so its name reads the way this version writes them.
                if let Some(chosen) = self.chosen[at].as_mut() {
                    if let Some(fresh) = list.iter().find(|s| s.id == chosen.id) {
                        *chosen = fresh.clone();
                    }
                }
                self.lists[at] = Some(list);
            }
            Err(why) => self.list_failed = Some(why),
        }
        if at == self.service {
            self.fill_picker();
        }
        true
    }

    fn open_picker(&mut self, what: Listing) {
        // Ookla's places are the app's own table; anything else is in a list.
        let from_table = self.current() == Service::Ookla && what != Listing::Server;
        if !from_table {
            self.fetch_list();
        }
        self.picker = Some(Picker { what, items: Vec::new(), selected: 0, scroll: 0 });
        self.fill_picker();
    }

    /// The list's rows, from what is known, with the cursor on what is chosen.
    fn fill_picker(&mut self) {
        let Some(what) = self.picker.as_ref().map(|p| p.what) else {
            return;
        };
        // iperf3 with nowhere chosen has no Auto: there is no server of its own.
        let auto = self.current() != Service::Iperf || what != Listing::Server || self.placed();
        let mut items = if auto { vec![Pick::Auto] } else { Vec::new() };
        let from_table = self.current() == Service::Ookla && what != Listing::Server;
        match what {
            _ if from_table => items.extend(self.place_names(what).into_iter().map(Pick::Name)),
            _ => match &self.lists[self.service] {
                Some(_) => {
                    let found: Vec<Pick> = if what == Listing::Server {
                        let own = if self.current() == Service::Iperf {
                            self.own_hosts()
                        } else {
                            Vec::new()
                        };
                        self.listed_servers().into_iter().chain(own).map(Pick::Server).collect()
                    } else {
                        self.place_names(what).into_iter().map(Pick::Name).collect()
                    };
                    if found.is_empty() && self.current() != Service::Iperf {
                        items.push(Pick::Note("No servers found".into()));
                    }
                    items.extend(found);
                    if what == Listing::Server && self.current() == Service::Iperf {
                        items.push(Pick::Enter);
                    }
                }
                None if self.loading.is_some() => {
                    items.push(Pick::Note("Loading servers...".into()))
                }
                None => items.extend(self.list_failed.clone().map(Pick::Note)),
            },
        }
        let chosen = self.chosen[self.service].as_ref().map(|s| s.id.clone());
        let country = self.country[self.service].clone();
        let town = self.town[self.service].clone();
        let selected = items
            .iter()
            .position(|p| match (p, what) {
                (Pick::Server(s), _) => Some(&s.id) == chosen.as_ref(),
                (Pick::Name(n), Listing::Country) => Some(n) == country.as_ref(),
                (Pick::Name(n), _) => Some(n) == town.as_ref(),
                (Pick::Auto, Listing::Country) => country.is_none(),
                (Pick::Auto, Listing::Town) => town.is_none(),
                (Pick::Auto, Listing::Server) => chosen.is_none(),
                _ => false,
            })
            .unwrap_or(0);
        if let Some(picker) = self.picker.as_mut() {
            picker.selected = selected;
            picker.scroll = scroll_for(selected, picker.scroll, items.len());
            picker.items = items;
        }
    }

    fn pick(&mut self) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let what = picker.what;
        match (picker.items.get(picker.selected).cloned(), what) {
            (Some(Pick::Auto), Listing::Server) => self.choose(None),
            (Some(Pick::Server(server)), _) => self.choose(Some(server)),
            (Some(Pick::Auto), Listing::Country) => self.set_country(None),
            (Some(Pick::Auto), Listing::Town) => self.set_town(None),
            (Some(Pick::Name(name)), Listing::Country) => self.set_country(Some(name)),
            (Some(Pick::Name(name)), _) => self.set_town(Some(name)),
            (Some(Pick::Enter), _) => {
                // Over the list, which is where Cancel comes back to.
                let current = self.chosen[self.service].as_ref().filter(|s| s.country.is_empty());
                let current = current.map_or("", |s| s.id.as_str());
                self.kb = Some(keyboard::TextInput::new("iperf3 server", current));
                return;
            }
            _ => return,
        }
        self.picker = None;
    }

    fn choose(&mut self, server: Option<Server>) {
        self.chosen[self.service] = server;
        self.picker = None;
        self.save();
    }

    fn enter_host(&mut self, host: &str) {
        let host = host.trim();
        if host.is_empty() {
            return;
        }
        self.hosts.retain(|h| h != host);
        self.hosts.insert(0, host.to_string());
        self.hosts.truncate(KEPT_HOSTS);
        self.choose(Some(Server { id: host.into(), name: host.into(), ..Default::default() }));
    }

    fn title(row: Row) -> &'static str {
        match row {
            Row::Service => "Service",
            Row::Town => "City",
            Row::Country => "Country",
            Row::Server => "Server",
        }
    }

    fn value(&self, row: Row) -> String {
        match row {
            Row::Service => self.current().name().into(),
            Row::Country => self.country[self.service].clone().unwrap_or_else(|| "Auto".into()),
            Row::Town => self.town[self.service].clone().unwrap_or_else(|| "Auto".into()),
            Row::Server => match (&self.chosen[self.service], self.current()) {
                (Some(server), _) => self.server_text(server),
                (None, Service::Cloudflare) => "Nearest".into(),
                (None, Service::Iperf) if !self.placed() => "Not set".into(),
                (None, _) => "Auto".into(),
            },
        }
    }

    /// Whether Left and Right change the row, which is what its chevrons say.
    fn adjustable(&self, row: Row) -> bool {
        !(row == Row::Server && self.current() == Service::Cloudflare)
    }

    fn settings(&self) -> Vec<ListItem> {
        self.rows()
            .into_iter()
            .map(|row| setting(Self::title(row), &self.value(row), self.adjustable(row)))
            .collect()
    }

    fn figure(&self, phase: Phase) -> String {
        let r = &self.results;
        let (value, dir) = match phase {
            Phase::Ping => (r.ping.map(ms), None),
            Phase::Down => (r.down.map(mbps), Some(Dir::Down)),
            Phase::Up => (r.up.map(mbps), Some(Dir::Up)),
        };
        if let Some(value) = value {
            return value;
        }
        if let (Some((live, rate)), Some(dir)) = (r.live, dir) {
            if live == dir {
                return mbps(rate);
            }
        }
        if self.phase() == Some(phase) {
            MEASURING.into()
        } else {
            NONE.into()
        }
    }

    /// The results page: the server the test reached, and the four figures.
    fn figures(&self) -> Vec<DetailRow> {
        let jitter = match (self.results.jitter, self.phase()) {
            (Some(j), _) => ms(j),
            (None, Some(Phase::Ping)) if self.current() != Service::Cloudflare => MEASURING.into(),
            _ => NONE.into(),
        };
        let server = self.results.server.clone().unwrap_or_else(|| self.value(Row::Server));
        vec![
            detail(0, "Server", &server),
            detail(1, "", ""),
            detail(0, "Ping", &self.figure(Phase::Ping)),
            detail(0, "Jitter", &jitter),
            detail(0, "Download", &self.figure(Phase::Down)),
            detail(0, "Upload", &self.figure(Phase::Up)),
        ]
    }

    fn dialog(&self) -> Vec<SharedString> {
        let Some(why) = self.failed.as_ref() else {
            return Vec::new();
        };
        // Inside the frame's own corners.
        let room = metric::MODAL_W - radius::BOX * 2;
        vec!["Test failed".into(), font::fit(&font::ascii(why), room).into()]
    }
}

fn apply_event(results: &mut Results, event: Event) {
    match event {
        Event::Server(name) => results.server = Some(name),
        Event::Ping { ms, jitter } => {
            results.ping = Some(ms);
            results.jitter = jitter;
        }
        Event::Live(dir, rate) => results.live = Some((dir, rate)),
        Event::Done(dir, rate) => {
            match dir {
                Dir::Down => results.down = Some(rate),
                Dir::Up => results.up = Some(rate),
            }
            if results.live.is_some_and(|(d, _)| d == dir) {
                results.live = None;
            }
        }
        Event::Failed(_) => {}
    }
}

/// `font::fit`, in a face other than the title's.
fn fit_with(text: &str, budget: i32, width: impl Fn(&str) -> i32) -> String {
    if width(text) <= budget {
        return text.to_string();
    }
    let mut out = String::new();
    for c in text.chars() {
        out.push(c);
        if width(&out) + width("..") > budget {
            out.pop();
            break;
        }
    }
    out.trim_end().to_string() + ".."
}

fn row_w(text: &str) -> i32 {
    i32::from(font::ROW.text_width(text))
}

fn active_w(text: &str) -> i32 {
    i32::from(font::ROW_ACTIVE.text_width(text))
}

/// Where a menu row's label starts: past the room its icon would have.
fn label_x() -> i32 {
    metric::ICON_PAD + 14 + metric::ICON_TEXT_GAP
}

/// What ChevronStatus puts around a value: a chevron each side, each a gap away.
fn chevrons_w() -> i32 {
    row_w("<") + row_w(">") + 2 * metric::VALUE_SPACING
}

/// A settings row. An adjustable one carries its value between chevrons, which is
/// the room it is cut to: the row's own width less its label, in the bold face it
/// takes when selected, and less the two chevrons ChevronStatus lays out from the
/// row's right edge inwards.
fn setting(label: &str, value: &str, adjustable: bool) -> ListItem {
    let chevrons = if adjustable { chevrons_w() } else { 0 };
    let room = metric::LIST_CONTAINER_W
        - label_x()
        - active_w(label)
        - metric::TABLE_GAP
        - metric::STATUS_PAD_R
        - chevrons;
    let value = fit_with(&font::ascii(value), room, row_w);
    ListItem {
        label: label.into(),
        tag: "".into(),
        status: if adjustable { "".into() } else { value.as_str().into() },
        icon: 0,
        picture: slint::Image::default(),
        frames: 1,
        chevrons: if adjustable { 2 } else { 0 },
        dim: false,
        value: if adjustable { value.into() } else { "".into() },
        at_start: false,
        at_end: false,
    }
}

/// A results row, its value cut to what the row leaves beside its label.
fn detail(kind: i32, label: &str, value: &str) -> DetailRow {
    let room = flipctl_app::theme::PANEL_W as i32
        - 2 * metric::MARGIN_H
        - row_w(label)
        - metric::TABLE_GAP;
    let value = fit_with(&font::ascii(value), room, row_w);
    DetailRow { kind, label: label.into(), value: value.into(), percent: 0, dim: false }
}

fn ms(value: f64) -> String {
    match value {
        v if v < 10.0 => format!("{v:.2} ms"),
        v if v < 100.0 => format!("{v:.1} ms"),
        v => format!("{v:.0} ms"),
    }
}

fn mbps(value: f64) -> String {
    if value >= 1000.0 {
        format!("{:.2} Gbps", value / 1000.0)
    } else {
        format!("{value:.1} Mbps")
    }
}

/// The first visible row, moved only as far as keeps `selected` on screen.
fn scroll_for(selected: usize, scroll: usize, total: usize) -> usize {
    let rows = count::LIST_VISIBLE_ROWS as usize;
    let scroll = if selected < scroll {
        selected
    } else if selected >= scroll + rows {
        selected + 1 - rows
    } else {
        scroll
    };
    scroll.min(total.saturating_sub(rows))
}

fn list_item(app: &App, pick: &Pick, what: Listing) -> ListItem {
    let (label, status) = match (pick, what) {
        // iperf3's tries the place's servers in turn, and takes the first that
        // answers.
        (Pick::Auto, Listing::Server) if app.current() == Service::Iperf => {
            ("Auto".to_string(), "first free".to_string())
        }
        (Pick::Auto, Listing::Server) => ("Auto".to_string(), "nearest".to_string()),
        // Ookla's Auto is where the user is, or the capital of the country chosen;
        // LibreSpeed's is every server.
        (Pick::Auto, Listing::Town)
            if app.current() == Service::Ookla && app.country[0].is_some() =>
        {
            ("Auto".to_string(), "capital".to_string())
        }
        (Pick::Auto, _) if app.current() == Service::Ookla => {
            ("Auto".to_string(), "your location".to_string())
        }
        (Pick::Auto, _) => ("Auto".to_string(), "all".to_string()),
        (Pick::Server(s), _) => (app.server_text(s), s.note.clone()),
        (Pick::Name(name), _) => (name.clone(), String::new()),
        (Pick::Enter, _) => ("Enter address".to_string(), String::new()),
        (Pick::Note(text), _) => (text.clone(), String::new()),
    };
    // The label is drawn bold when selected, so it is cut in the wider face, and
    // short of the status at the other edge.
    let room = metric::LIST_CONTAINER_W
        - label_x()
        - metric::STATUS_PAD_R
        - row_w(&status)
        - metric::TABLE_GAP;
    let label = fit_with(&font::ascii(&label), room, active_w);
    ListItem {
        label: label.into(),
        tag: "".into(),
        status: status.into(),
        icon: 0,
        picture: slint::Image::default(),
        frames: 1,
        chevrons: 0,
        dim: true,
        value: "".into(),
        at_start: false,
        at_end: false,
    }
}

fn strings(items: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(items.iter().map(|s| SharedString::from(*s)).collect::<Vec<_>>()))
}

/// Put the state on screen. Every frame is built from the state and nothing is
/// updated in place, so what is drawn cannot disagree with what is true.
fn apply(ui: &AppWindow, app: &App) {
    ui.set_settings(ModelRc::new(VecModel::from(app.settings())));
    ui.set_setting_row(app.row as i32);
    ui.set_arrow_pressed(app.arrow);
    ui.set_results(app.page == Page::Results);
    ui.set_figures(ModelRc::new(VecModel::from(app.figures())));
    ui.set_run_label(if app.running() { "Stop".into() } else { "Start".into() });
    ui.set_dialog(ModelRc::new(VecModel::from(app.dialog())));
    ui.set_dialog_right(if app.failed.is_some() { "Ok".into() } else { "".into() });

    ui.set_listing(app.picker.is_some());
    if let Some(picker) = app.picker.as_ref() {
        ui.set_items(ModelRc::new(VecModel::from(
            picker.items.iter().map(|p| list_item(app, p, picker.what)).collect::<Vec<_>>(),
        )));
        ui.set_selected(picker.selected as i32);
        ui.set_scroll(picker.scroll as i32);
    }
    apply_kb(ui, app);
}

/// The keyboard's half of the screen, as the browser puts it up.
fn apply_kb(ui: &AppWindow, app: &App) {
    ui.set_keyboard(app.kb.is_some());
    let Some(field) = app.kb.as_ref() else {
        return;
    };
    let v = field.view("", field.cursor_visible());
    ui.set_kb_title(v.title.as_str().into());
    ui.set_kb_text(v.text.as_str().into());
    ui.set_kb_field_w(v.field_w);
    ui.set_kb_cursor_dx(v.cursor_dx);
    ui.set_kb_cursor_on(v.cursor_on);
    ui.set_kb_field_focused(v.field_focused);
    ui.set_kb_warning(v.warning.as_str().into());
    let cells: Vec<KbCell> = v
        .cells
        .iter()
        .map(|c| KbCell {
            x: c.x as f32,
            y: c.y as f32,
            w: c.w as f32,
            text: c.text.as_str().into(),
            icon: c.icon,
            icon_w: c.icon_w as f32,
            icon_h: c.icon_h as f32,
            selected: c.selected,
            pressed: c.pressed,
            clip_h: c.clip_h as f32,
        })
        .collect();
    ui.set_kb_cells(ModelRc::new(VecModel::from(cells)));
    ui.set_kb_chrome_x(v.chrome.0);
    ui.set_kb_chrome_y(v.chrome.1);
    ui.set_kb_chrome_w(v.chrome.2);
    ui.set_kb_chrome_h(v.chrome.3);
    ui.set_kb_lang_label(v.lang_label.into());
    ui.set_kb_tab_label(v.tab_label.into());
    ui.set_kb_tab_focus(v.tab_focus);
    ui.set_kb_tab_pressed(v.tab_pressed);
    ui.set_kb_discard(v.discard);
    ui.set_kb_buttons(strings(&["Cancel", "", "", "", "Done"]));
}

/// One key. Returns false when the app should close.
fn key(app: &mut App, key: Key, down: bool) -> bool {
    if let Some(field) = app.kb.as_mut() {
        if !down {
            field.release(key.flipper());
            return true;
        }
        match field.key(key.flipper(), true) {
            Some(keyboard::Exit::Save(text)) => {
                app.kb = None;
                app.enter_host(&text);
            }
            Some(keyboard::Exit::Cancel) => app.kb = None,
            None => {}
        }
        return true;
    }
    if !down {
        // The chevron blinks for as long as it is held, as on flipctl's own rows.
        if matches!(key, Key::Left | Key::Right) {
            app.arrow = 0;
        }
        return true;
    }

    // The dialog owns the keys while it is up, and any of them dismisses it.
    if app.failed.is_some() {
        if matches!(key, Key::Ok | Key::Run | Key::Back | Key::Escape) {
            app.failed = None;
        }
        return true;
    }

    if let Some(picker) = app.picker.as_mut() {
        let total = picker.items.len().max(1);
        match key {
            Key::Back | Key::Escape => app.picker = None,
            Key::Down => picker.selected = (picker.selected + 1) % total,
            Key::Up => picker.selected = (picker.selected + total - 1) % total,
            Key::Ok | Key::Run => app.pick(),
            _ => {}
        }
        if let Some(picker) = app.picker.as_mut() {
            picker.scroll = scroll_for(picker.selected, picker.scroll, picker.items.len());
        }
        return true;
    }

    if app.page == Page::Results {
        match key {
            // Back is back to the settings, and takes a test under way with it.
            Key::Back | Key::Escape => {
                app.stop();
                app.page = Page::Settings;
            }
            Key::Run | Key::Ok if app.running() => app.stop(),
            Key::Run | Key::Ok => app.start(),
            _ => {}
        }
        return true;
    }

    let rows = app.rows();
    let row = rows[app.row];
    match key {
        Key::Back | Key::Escape => return false,
        Key::Run => app.start(),
        Key::Down => app.row = (app.row + 1) % rows.len(),
        Key::Up => app.row = (app.row + rows.len() - 1) % rows.len(),
        Key::Left | Key::Right if app.adjustable(row) => {
            let forward = key == Key::Right;
            app.arrow = if forward { 2 } else { 1 };
            app.cycle(row, if forward { 1 } else { -1 });
        }
        Key::Ok => app.open(row),
        _ => {}
    }
    true
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    // Kept only where flipctl runs the app, in the directory it gives it: a run from
    // a checkout leaves nothing behind.
    let store = std::env::var_os("FLIPCTL_HOSTED").map(|_| PathBuf::from(STORE));
    let state = Rc::new(RefCell::new(App::new(store)));
    apply(&ui, &state.borrow());

    let mut status = StatusSource::new(Duration::from_secs(2));
    flipctl_app::apply_status!(&ui, PanelStatus, status.current());

    let tick = slint::Timer::default();
    let ticking = ui.as_weak();
    let ticked = Rc::clone(&state);
    tick.start(slint::TimerMode::Repeated, TICK, move || {
        let Some(ui) = ticking.upgrade() else {
            return;
        };
        let mut app = ticked.borrow_mut();
        let mut changed = app.poll();
        changed |= app.poll_list();
        if let Some(field) = app.kb.as_mut() {
            field.animating();
            // The caret blinks on the clock, so the field is redrawn while it is up.
            apply_kb(&ui, &app);
        }
        if changed {
            apply(&ui, &app);
        }
        if let Some(now) = status.poll() {
            flipctl_app::apply_status!(&ui, PanelStatus, now);
        }
    });

    let keys = ui.as_weak();
    let keyed = Rc::clone(&state);
    ui.on_keyed(move |text, down| {
        let Some(ui) = keys.upgrade() else {
            return;
        };
        let Some(pressed) = Key::from_slint(text.as_str()) else {
            return;
        };
        ui.set_pressed_slot(match (down, pressed.soft_slot()) {
            (true, Some(slot)) => slot as i32,
            _ => -1,
        });
        let mut app = keyed.borrow_mut();
        if !key(&mut app, pressed, down) {
            let _ = slint::quit_event_loop();
            return;
        }
        apply(&ui, &app);
    });

    ui.run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flipctl_app::theme;
    use flipper_ui::slint_render::{render_frame, FlipperSlintPlatform};
    use slint::platform::software_renderer::MinimalSoftwareWindow;

    /// The panel, once per thread: installing the platform twice is a panic.
    fn panel() -> Rc<MinimalSoftwareWindow> {
        thread_local! {
            static WINDOW: Rc<MinimalSoftwareWindow> = FlipperSlintPlatform::install();
        }
        WINDOW.with(Rc::clone)
    }

    /// Draw the page as the panel would receive it. `SPEEDTEST_RENDER=1 cargo test`
    /// leaves the frames in target/render to look at.
    fn shot(name: &str, app: &App) -> Vec<u8> {
        let window = panel();
        let ui = AppWindow::new().expect("create AppWindow");
        apply(&ui, app);
        ui.show().expect("show");
        slint::platform::update_timers_and_animations();
        let frame: Vec<u8> = render_frame(&window).expect("frame").iter().map(|px| px.0).collect();
        drop(ui);
        if std::env::var_os("SPEEDTEST_RENDER").is_some() {
            save(name, &frame);
        }
        frame
    }

    fn save(name: &str, frame: &[u8]) {
        let dir = std::path::Path::new("target/render");
        std::fs::create_dir_all(dir).expect("create target/render");
        let file = std::fs::File::create(dir.join(format!("{name}.png"))).expect("create png");
        let mut encoder = png::Encoder::new(
            std::io::BufWriter::new(file),
            u32::from(theme::PANEL_W),
            u32::from(theme::PANEL_H),
        );
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().expect("png header").write_image_data(frame).expect("png data");
    }

    /// Invariant 1: every pixel holds a design-token value. Under a dialog, the
    /// scrim's exact composite of one is allowed too, as flipctl's own tests allow
    /// it: 75% white over the page, the status bar included.
    fn only_tokens(frame: &[u8], scrim: bool) {
        let tokens: Vec<u8> = theme::ALL_COLORS.iter().map(|(_, _, r, ..)| *r).collect();
        // The renderer applies the alpha as an 8-bit value, so the composite is
        // worked out the same way.
        let alpha = (theme::alpha::OVERLAY * 255.0).round() as u32;
        let dimmed = |over: u8| {
            let c = u32::from(theme::color::OVERLAY.0) * alpha + u32::from(over) * (255 - alpha);
            (c as f32 / 255.0).round() as u8
        };
        let allowed: Vec<u8> = tokens
            .iter()
            .copied()
            .chain(tokens.iter().copied().filter(|_| scrim).map(dimmed))
            .collect();
        let stride = i32::from(theme::PANEL_W);
        let offenders: Vec<String> = frame
            .iter()
            .enumerate()
            .filter(|(_, px)| !allowed.contains(px))
            .take(8)
            .map(|(i, px)| format!("({}, {}) = {px}", i as i32 % stride, i as i32 / stride))
            .collect();
        assert!(offenders.is_empty(), "off-palette pixels: {offenders:?}");
    }

    fn app() -> App {
        App::new(None)
    }

    /// A state whose lists owe nothing to the machine the test runs on.
    fn known() -> App {
        let mut app = app();
        app.lists[0] = Some(Service::Ookla.parse_list(include_str!("../fixtures/ookla-list.txt")));
        app.lists[1] =
            Some(Service::LibreSpeed.parse_list(include_str!("../fixtures/librespeed-list.txt")));
        app
    }

    fn finished() -> App {
        let mut app = known();
        app.page = Page::Results;
        let r = &mut app.results;
        r.server = Some("NETOPS, Belgrade".into());
        r.ping = Some(2.91429);
        r.jitter = Some(0.076573);
        r.down = Some(148.74);
        r.up = Some(366.93);
        app
    }

    fn figure(app: &App, label: &str) -> String {
        app.figures().iter().find(|r| r.label == label).expect(label).value.to_string()
    }

    fn setting_value(app: &App, label: &str) -> String {
        let item = app.settings().into_iter().find(|i| i.label == label).expect(label);
        if item.chevrons == 2 { item.value } else { item.status }.to_string()
    }

    fn sleeping() -> Test {
        let argv = vec!["sleep".to_string(), "5".to_string()];
        Test {
            runs: vec![argv.clone()],
            at: 0,
            targets: Vec::new(),
            target: 0,
            run: child::Run::start(&argv).expect("sleep"),
            failed: None,
            last: None,
        }
    }

    #[test]
    fn the_pages_are_drawn_only_in_tokens() {
        only_tokens(&shot("settings", &known()), false);
        let mut iperf = known();
        iperf.service = 3;
        iperf.row = 1;
        only_tokens(&shot("settings-iperf", &iperf), false);
        only_tokens(&shot("results", &finished()), false);

        // The longest names each list has, on the row and off it.
        let mut long = known();
        long.chosen[0] = Some(Server {
            id: "1".into(),
            name: "Airband Community Internet, London".into(),
            ..Default::default()
        });
        long.row = 3;
        only_tokens(&shot("long-ookla-selected", &long), false);
        long.row = 0;
        only_tokens(&shot("long-ookla", &long), false);
        long.service = 1;
        long.chosen[1] =
            long.lists[1].as_ref().unwrap().iter().max_by_key(|s| row_w(&s.name)).cloned();
        long.row = 3;
        only_tokens(&shot("long-librespeed-selected", &long), false);
        long.set_country(Some("Germany".into()));
        long.row = 1;
        only_tokens(&shot("librespeed-country", &long), false);

        let mut cities = known();
        cities.set_country(Some("United Kingdom".into()));
        cities.open_picker(Listing::Town);
        only_tokens(&shot("cities", &cities), false);
        let mut servers = known();
        servers.service = 1;
        servers.open_picker(Listing::Server);
        for _ in 0..7 {
            key(&mut servers, Key::Down, true);
        }
        only_tokens(&shot("servers", &servers), false);

        let mut running = known();
        running.page = Page::Results;
        running.test = Some(sleeping());
        apply_event(&mut running.results, Event::Ping { ms: 31.53, jitter: None });
        apply_event(&mut running.results, Event::Live(Dir::Down, 419.16));
        only_tokens(&shot("running", &running), false);

        let mut failed = finished();
        failed.failed = Some("Failed retrieving user information".into());
        only_tokens(&shot("dialog", &failed), true);
    }

    #[test]
    fn ookla_and_librespeed_have_place_rows() {
        let mut app = known();
        let labels: Vec<String> = app.settings().iter().map(|i| i.label.to_string()).collect();
        assert_eq!(labels, ["Service", "Country", "City", "Server"]);
        app.row = 3;
        key(&mut app, Key::Right, true);
        assert_eq!(app.current(), Service::Ookla, "Right on the server row moves the server");
        app.row = 0;
        key(&mut app, Key::Right, true);
        assert_eq!(app.current(), Service::LibreSpeed);
        let labels: Vec<String> = app.settings().iter().map(|i| i.label.to_string()).collect();
        assert_eq!(labels, ["Service", "Country", "City", "Server"]);
        key(&mut app, Key::Right, true);
        assert_eq!(app.current(), Service::Cloudflare);
        let labels: Vec<String> = app.settings().iter().map(|i| i.label.to_string()).collect();
        assert_eq!(labels, ["Service", "Server"]);
    }

    #[test]
    fn an_ookla_place_moves_the_server_list_and_lets_go_of_the_server() {
        let mut app = known();
        for row in ["Country", "City", "Server"] {
            assert_eq!(setting_value(&app, row), "Auto", "{row} starts on Auto");
        }
        assert_eq!(app.test_runs().0, vec![vec!["speedtest-go"]], "Auto is where the user is");
        app.row = 3;
        key(&mut app, Key::Right, true);
        assert_eq!(app.chosen[0].as_ref().unwrap().id, "69116", "the first server after Auto");

        // Countries out of the city table, not out of any list: nothing is fetched.
        app.row = 1;
        key(&mut app, Key::Ok, true);
        assert!(app.loading.is_none());
        let items = app.picker.as_ref().unwrap().items.clone();
        assert!(items.len() > 200, "{}", items.len());
        let uk = items.iter().position(|p| *p == Pick::Name("United Kingdom".into())).unwrap();
        app.picker.as_mut().unwrap().selected = uk;
        key(&mut app, Key::Ok, true);
        assert_eq!(setting_value(&app, "Country"), "United Kingdom");
        assert_eq!(setting_value(&app, "Server"), "Auto", "a Belgrade server is not in the UK");
        assert!(app.lists[0].is_none(), "the list is the UK's now, so it is fetched again");
        // With the city on Auto, the country's capital is where Ookla is asked.
        assert_eq!(app.test_runs().0[0].join(" "), "speedtest-go --location=51.5085,-0.1257");

        // Cities, the UK's own.
        app.row = 2;
        key(&mut app, Key::Ok, true);
        let towns = app.picker.as_ref().unwrap().items.clone();
        let auto = list_item(&app, &towns[0], Listing::Town);
        assert_eq!(auto.status, "capital", "Auto in a country chosen is its capital");
        assert!(towns.contains(&Pick::Name("Manchester".into())));
        assert!(!towns.contains(&Pick::Name("Berlin".into())));
        app.picker = None;
        app.set_town(Some("Manchester".into()));
        assert_eq!(app.test_runs().0[0].join(" "), "speedtest-go --location=53.4809,-2.2374");
        assert_eq!(
            Service::Ookla.list_args(app.ookla_place()).join(" "),
            "speedtest-go --list --location=53.4809,-2.2374"
        );

        // And the chevrons step through the cities too, then back to Auto.
        app.set_town(None);
        key(&mut app, Key::Left, true);
        assert_ne!(setting_value(&app, "City"), "Auto");
        app.set_country(None);
        assert_eq!(setting_value(&app, "City"), "Auto");
        assert_eq!(app.test_runs().0, vec![vec!["speedtest-go"]]);
    }

    #[test]
    fn librespeed_narrows_by_country_then_city() {
        let mut app = known();
        app.service = 1;
        let labels: Vec<String> = app.settings().iter().map(|i| i.label.to_string()).collect();
        assert_eq!(labels, ["Service", "Country", "City", "Server"]);
        for row in ["Country", "City", "Server"] {
            assert_eq!(setting_value(&app, row), "Auto", "{row} starts on Auto");
        }
        assert_eq!(
            app.test_runs().0,
            vec![vec!["librespeed-cli"]],
            "Auto everywhere is the program's own choice"
        );

        // Country, from the list's own, with its two spellings of one country as one.
        app.row = 1;
        key(&mut app, Key::Ok, true);
        let countries: Vec<Pick> = app.picker.as_ref().unwrap().items.clone();
        assert_eq!(countries[0], Pick::Auto);
        assert!(countries.contains(&Pick::Name("Germany".into())));
        assert!(!countries.contains(&Pick::Name("USA".into())), "USA is the United States");
        let germany = countries.iter().position(|p| *p == Pick::Name("Germany".into())).unwrap();
        app.picker.as_mut().unwrap().selected = germany;
        key(&mut app, Key::Ok, true);
        assert_eq!(setting_value(&app, "Country"), "Germany");

        // Auto inside it: the program chooses between Germany's servers only.
        let runs = app.test_runs().0;
        assert_eq!(runs[0].join(" "), "librespeed-cli --server 50 --server 105");

        // City, out of that country's.
        app.row = 2;
        key(&mut app, Key::Ok, true);
        assert_eq!(
            app.picker.as_ref().unwrap().items,
            vec![Pick::Auto, Pick::Name("Frankfurt".into())]
        );
        key(&mut app, Key::Down, true);
        key(&mut app, Key::Ok, true);
        assert_eq!(setting_value(&app, "City"), "Frankfurt");

        // The servers there, by sponsor alone, since the rows above say the rest.
        app.row = 3;
        key(&mut app, Key::Right, true);
        assert_eq!(setting_value(&app, "Server"), "Clouvider");
        assert_eq!(app.test_runs().0[0].join(" "), "librespeed-cli --server 50");

        // Another country lets go of the city and of a server that is not in it.
        app.row = 1;
        key(&mut app, Key::Right, true);
        assert_ne!(setting_value(&app, "Country"), "Germany");
        assert_eq!(setting_value(&app, "City"), "Auto");
        assert_eq!(setting_value(&app, "Server"), "Auto");
    }

    #[test]
    fn a_saved_server_takes_its_name_from_the_list() {
        let mut app = app();
        app.service = 1;
        app.chosen[1] = Some(Server {
            id: "105".into(),
            name: "Frankfurt, Germany (FS IT-Systeme GmbH)".into(),
            ..Default::default()
        });
        let (tx, rx) = mpsc::channel();
        tx.send(
            Ok(Service::LibreSpeed.parse_list(include_str!("../fixtures/librespeed-list.txt"))),
        )
        .unwrap();
        app.loading = Some((1, rx));
        assert!(app.poll_list());
        assert_eq!(app.chosen[1].as_ref().unwrap().name, "Germany, Frankfurt (FS IT-Systeme GmbH)");
    }

    fn iperf() -> App {
        let mut app = app();
        app.service = 3;
        app
    }

    #[test]
    fn an_iperf_host_is_typed_from_the_end_of_the_server_list() {
        let mut app = iperf();
        app.row = 3;
        key(&mut app, Key::Ok, true);
        let items = app.picker.as_ref().expect("the list").items.clone();
        assert_ne!(items[0], Pick::Auto, "no Auto with nowhere chosen");
        assert_eq!(items.last(), Some(&Pick::Enter));
        app.picker.as_mut().unwrap().selected = items.len() - 1;
        key(&mut app, Key::Ok, true);
        assert!(app.kb.is_some(), "Enter address types a host");
        only_tokens(&shot("keyboard", &app), false);
        app.kb = None;
        app.picker = None;
        app.enter_host(" 192.0.2.20 ");
        app.enter_host("nas.lan:5202");
        assert_eq!(app.hosts, vec!["nas.lan:5202", "192.0.2.20"]);
        assert_eq!(setting_value(&app, "Server"), "nas.lan:5202");
        assert_eq!(app.test_runs().1, [("nas.lan:5202".to_string(), "nas.lan:5202".to_string())]);

        // A typed host is offered after the listed servers, wherever the user is.
        app.set_country(Some("Serbia".into()));
        assert_eq!(setting_value(&app, "Server"), "nas.lan:5202", "a host typed in stays");
        app.open_picker(Listing::Server);
        let items = app.picker.as_ref().unwrap().items.clone();
        let own: Vec<String> = items
            .iter()
            .filter_map(|p| match p {
                Pick::Server(s) if s.note == "yours" => Some(s.id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(own, ["nas.lan:5202", "192.0.2.20"]);
        // And the keyboard opens on the host typed last, not on a listed server.
        app.picker.as_mut().unwrap().selected = items.len() - 1;
        key(&mut app, Key::Ok, true);
        assert_eq!(app.kb.as_ref().unwrap().text, "nas.lan:5202");
    }

    #[test]
    fn iperf_narrows_the_public_servers_by_place() {
        let mut app = iperf();
        let labels: Vec<String> = app.settings().iter().map(|i| i.label.to_string()).collect();
        assert_eq!(labels, ["Service", "Country", "City", "Server"]);
        assert_eq!(setting_value(&app, "Server"), "Not set");
        app.set_country(Some("United Kingdom".into()));
        assert_eq!(setting_value(&app, "Server"), "Auto");
        app.open_picker(Listing::Town);
        let towns = app.picker.as_ref().unwrap().items.clone();
        assert_eq!(
            towns,
            [
                Pick::Auto,
                Pick::Name("London".into()),
                Pick::Name("Maidenhead".into()),
                Pick::Name("Manchester".into())
            ]
        );
        app.set_town(Some("London".into()));
        app.open_picker(Listing::Server);
        only_tokens(&shot("iperf-servers", &app), false);
        app.set_town(None);
        app.picker = None;
        app.set_town(Some("Manchester".into()));
        // Auto in a place: its servers, one port each, in turn.
        let targets = app.test_runs().1;
        assert_eq!(targets.len(), 1);
        assert!(targets[0].0.starts_with("man.speedtest.clouvider.net:52"), "{targets:?}");
        assert_eq!(targets[0].1, "Clouvider");
        app.set_town(Some("London".into()));
        assert!(app.test_runs().1.len() > 3, "London has several to fall back on");
        // One server chosen: a few of its ports.
        app.set_town(Some("Maidenhead".into()));
        let chosen = app.listed_servers().into_iter().find(|s| s.sponsor == "RapidSwitch");
        assert!(chosen.is_some());
        app.choose(chosen);
        let (runs, targets) = app.test_runs();
        assert_eq!(targets.len(), 4);
        assert!(targets
            .iter()
            .all(|(t, l)| t.starts_with("speedtest.rapidswitch.com:520") && l == "RapidSwitch"));
        assert_eq!(runs.len(), 2, "download, then upload");
    }

    #[test]
    fn a_busy_iperf_server_moves_the_test_on() {
        let mut app = iperf();
        app.page = Page::Results;
        let busy: Vec<String> = [
            "sh",
            "-c",
            "echo 'iperf3: error - the server is busy running a test. try again later'; exit 1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        app.test = Some(Test {
            runs: vec![busy.clone(), busy.clone()],
            at: 0,
            targets: vec![("a:5201".into(), "First".into()), ("b:5201".into(), "Second".into())],
            target: 0,
            run: child::Run::start(&busy).expect("sh"),
            failed: None,
            last: None,
        });
        // Busy: on to the second target, iperf3 itself this time, which is not
        // installed here and so ends the test with that said.
        for _ in 0..200 {
            app.poll();
            if app.test.as_ref().is_none_or(|t| t.target == 1) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.failed.is_some() || app.test.as_ref().is_some_and(|t| t.target == 1));
        assert_eq!(app.results.server.as_deref(), Some("Second"), "the page says where it went");
    }

    #[test]
    fn start_with_nowhere_for_iperf_asks_on_its_row() {
        let mut app = iperf();
        key(&mut app, Key::Run, true);
        assert!(app.picker.is_some(), "the server list, to choose or type one");
        assert_eq!(app.row, 3);
        assert_eq!(app.page, Page::Settings);
    }

    #[test]
    fn the_server_row_opens_a_listed_services_servers() {
        let mut app = known();
        app.service = 1;
        app.row = 3;
        key(&mut app, Key::Ok, true);
        let picker = app.picker.as_ref().expect("the list");
        assert_eq!(picker.items[0], Pick::Auto);
        key(&mut app, Key::Down, true);
        key(&mut app, Key::Ok, true);
        assert!(app.picker.is_none());
        let list = app.lists[1].clone().unwrap();
        assert_eq!(app.chosen[1].as_ref(), Some(&list[0]));
        key(&mut app, Key::Right, true);
        assert_eq!(app.chosen[1].as_ref(), Some(&list[1]));
        key(&mut app, Key::Left, true);
        key(&mut app, Key::Left, true);
        assert_eq!(setting_value(&app, "Server"), "Auto");
    }

    #[test]
    fn cloudflares_server_is_a_plain_value() {
        let mut app = known();
        app.service = 2;
        app.row = 1;
        let server = &app.settings()[1];
        assert_eq!(server.chevrons, 0, "nothing to step through, so no chevrons");
        assert_eq!(server.status, "Nearest");
        key(&mut app, Key::Ok, true);
        key(&mut app, Key::Right, true);
        assert!(app.picker.is_none() && app.kb.is_none());
        assert_eq!(app.arrow, 0, "no chevron to blink");
    }

    #[test]
    fn a_held_chevron_blinks_until_it_is_let_go() {
        let mut app = known();
        key(&mut app, Key::Left, true);
        assert_eq!(app.arrow, 1);
        key(&mut app, Key::Left, false);
        assert_eq!(app.arrow, 0);
    }

    #[test]
    fn back_from_the_results_stops_the_test_and_returns_to_the_settings() {
        let mut app = known();
        app.page = Page::Results;
        app.test = Some(sleeping());
        assert!(key(&mut app, Key::Back, true), "back is not close from the results");
        assert!(!app.running());
        assert_eq!(app.page, Page::Settings);
        assert!(!key(&mut app, Key::Back, true), "and close from the settings");
    }

    #[test]
    fn results_read_as_figures_with_units() {
        let app = finished();
        assert_eq!(figure(&app, "Server"), "NETOPS, Belgrade");
        assert_eq!(figure(&app, "Ping"), "2.91 ms");
        assert_eq!(figure(&app, "Jitter"), "0.08 ms");
        assert_eq!(figure(&app, "Download"), "148.7 Mbps");
        assert_eq!(figure(&app, "Upload"), "366.9 Mbps");
        assert_eq!(mbps(19600.0), "19.60 Gbps");
        assert_eq!(ms(31.53), "31.5 ms");
    }

    #[test]
    fn a_long_value_is_cut_to_what_its_row_leaves() {
        let item = setting("Server", "YUNET International D.O.O., Beograd", true);
        assert!(item.value.ends_with(".."), "{}", item.value);
        let used =
            label_x() + active_w("Server") + metric::TABLE_GAP + row_w(&item.value) + chevrons_w();
        assert!(used + metric::STATUS_PAD_R <= metric::LIST_CONTAINER_W, "{used}");
    }

    #[test]
    fn the_figure_being_measured_says_so_and_a_running_value_takes_its_place() {
        let mut app = app();
        app.page = Page::Results;
        app.test = Some(sleeping());
        assert_eq!(figure(&app, "Ping"), MEASURING);
        assert_eq!(figure(&app, "Download"), NONE);
        apply_event(&mut app.results, Event::Ping { ms: 3.0, jitter: None });
        assert_eq!(figure(&app, "Download"), MEASURING);
        apply_event(&mut app.results, Event::Live(Dir::Down, 419.16));
        assert_eq!(figure(&app, "Download"), "419.2 Mbps");
        apply_event(&mut app.results, Event::Done(Dir::Down, 389.01));
        assert_eq!(figure(&app, "Download"), "389.0 Mbps");
        assert_eq!(figure(&app, "Upload"), MEASURING);
        key(&mut app, Key::Run, true);
        assert!(!app.running());
        assert_eq!(app.page, Page::Results, "stopping keeps what was measured on screen");
        assert_eq!(figure(&app, "Upload"), NONE);
    }

    #[test]
    fn a_program_that_fails_says_why() {
        let mut app = app();
        let argv: Vec<String> =
            ["sh", "-c", "echo 'Fatal: retrieving user information, err: x'; exit 1"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        app.test = Some(Test {
            runs: vec![argv.clone()],
            at: 0,
            targets: Vec::new(),
            target: 0,
            run: child::Run::start(&argv).expect("sh"),
            failed: None,
            last: None,
        });
        while app.running() {
            app.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.failed.as_deref(), Some("Failed retrieving user information"));
        key(&mut app, Key::Down, true);
        assert!(app.failed.is_some(), "the dialog takes the keys");
        assert_eq!(app.row, 0);
        key(&mut app, Key::Ok, true);
        assert!(app.failed.is_none());
    }

    #[test]
    fn the_list_scrolls_to_keep_the_cursor_in_view() {
        assert_eq!(scroll_for(0, 0, 20), 0);
        assert_eq!(scroll_for(5, 0, 20), 1);
        assert_eq!(scroll_for(2, 4, 20), 2);
        assert_eq!(scroll_for(19, 0, 20), 15);
        assert_eq!(scroll_for(3, 0, 3), 0);
    }

    #[test]
    fn the_choices_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("speedtest-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(STORE);
        let mut app = App::new(Some(path.clone()));
        app.set_country(Some("Serbia".into()));
        app.set_town(Some("Novi Sad".into()));
        app.service = 1;
        app.choose(Some(Server {
            id: "106".into(),
            name: "Serbia, Belgrade (SOX)".into(),
            ..Default::default()
        }));
        app.service = 3;
        app.enter_host("nas.lan:5202");
        let back = App::new(Some(path));
        assert_eq!(back.current(), Service::Iperf);
        assert_eq!(back.country[0].as_deref(), Some("Serbia"));
        assert_eq!(back.town[0].as_deref(), Some("Novi Sad"));
        assert_eq!(back.chosen[1].as_ref().unwrap().name, "Serbia, Belgrade (SOX)");
        assert_eq!(back.chosen[3].as_ref().unwrap().id, "nas.lan:5202");
        assert_eq!(back.hosts, vec!["nas.lan:5202"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
