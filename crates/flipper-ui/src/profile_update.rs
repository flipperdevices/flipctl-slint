//! Updating every profile to a newer build, through `update-profile --every`.
//!
//! The tool does the work and owns every decision: which builds suit which profile,
//! what to fetch, how the user's changes are carried and how a failure is undone.
//! This side reads what it says. `update-profile --list --every` answers in lines a
//! program can read, one fact each, with check-profile's word as each profile's state:
//!
//! ```text
//! profile @Desktop Desktop 1230 booted
//! profile @Router Router 1219 packages
//! booted @Desktop
//! kept 1219
//! build 1235 dev 942021864 Desktop,Router
//! ```
//!
//! A run with `--progress` adds `@profile <name>` as each profile starts and
//! `@progress <percent> <step>` lines, and ends with a `result` line per profile.
//! Rollback is the booted profile's alone: the tool defaults to it.

use std::sync::mpsc::Sender;

/// A build some profile could move to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub number: u32,
    /// The build-scripts branch it was built from, which is what a channel is.
    pub branch: String,
    /// What would be downloaded for it, in bytes.
    pub bytes: u64,
    /// The kinds of profile that would move to it.
    pub kinds: Vec<String>,
}

/// One profile on the drive.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    /// The subvolume, e.g. `@Desktop`.
    pub name: String,
    /// Which stock it is built from, e.g. `Desktop`.
    pub kind: String,
    pub build: u32,
    /// check-profile's word: pristine, booted, customized, packages or system.
    pub state: String,
}

/// Everything the screen shows before anything is started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub profiles: Vec<Profile>,
    /// The booted profile's subvolume, when it is one of them.
    pub booted: Option<String>,
    /// The build the booted profile's last forced update kept, to roll back to.
    pub kept: Option<u32>,
    /// Newest first, as the tool lists them.
    pub offers: Vec<Offer>,
}

impl Listing {
    /// The profiles that move to `offer`, the way the tool decides it: an updatable
    /// state and an older build, or with `force` any of them its build has a stock for.
    pub fn going(&self, offer: &Offer, force: bool) -> Vec<&Profile> {
        self.profiles
            .iter()
            .filter(|p| offer.kinds.contains(&p.kind))
            .filter(|p| force || (updatable(&p.state) && p.build < offer.number))
            .collect()
    }
}

/// Whether the tool will update a profile check-profile called `word`. One whose packages
/// or system files changed it updates only with --force, which drops those changes.
pub fn updatable(word: &str) -> bool {
    matches!(word, "pristine" | "booted" | "customized")
}

/// How a run started from the screen is getting on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Percent done, and the step under way.
    Working(u8, String),
    /// Finished; a line per profile saying what became of it.
    Done(Vec<String>),
    Failed(String),
}

/// Read `update-profile --list --every`'s answer. Lines it does not know are skipped,
/// so the tool can say more than this reads.
pub fn parse_list(text: &str) -> Result<Listing, String> {
    let mut out = Listing::default();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.as_slice() {
            ["profile", name, kind, build, state] => out.profiles.push(Profile {
                name: (*name).into(),
                kind: (*kind).into(),
                build: build.parse().map_err(|_| format!("bad build number: {build}"))?,
                state: (*state).into(),
            }),
            ["booted", name] => out.booted = Some((*name).into()),
            ["kept", build] => out.kept = build.parse().ok(),
            ["build", number, branch, bytes, kinds] => {
                if let (Ok(number), Ok(bytes)) = (number.parse(), bytes.parse()) {
                    let kinds = kinds.split(',').map(String::from).collect();
                    out.offers.push(Offer { number, branch: (*branch).into(), bytes, kinds });
                }
            }
            _ => {}
        }
    }
    if out.profiles.is_empty() {
        return Err("update-profile found no profile to update".into());
    }
    Ok(out)
}

/// What became of one profile, from a `result` line of the run, or `None` for any
/// other line.
pub fn parse_result(line: &str) -> Option<String> {
    let f: Vec<&str> = line.strip_prefix("result ")?.split_whitespace().collect();
    let name = |p: &str| crate::boot::display_name(p);
    Some(match f.as_slice() {
        [p, "updated", from, to] => format!("{} {from} to {to}", name(p)),
        [p, "blocked", ..] => format!("{} blocked", name(p)),
        [p, "current", build] => format!("{} already at {build}", name(p)),
        [p, "unavailable"] => format!("{} not in this build", name(p)),
        [p, "failed"] => format!("{} failed", name(p)),
        _ => return None,
    })
}

/// A progress line, as percent and step, or `None` for any other line.
pub fn parse_progress(line: &str) -> Option<(u8, String)> {
    let rest = line.strip_prefix("@progress ")?;
    let (pct, step) = rest.split_once(' ')?;
    Some((pct.parse::<u8>().ok()?.min(100), step.trim().to_string()))
}

/// Ask the tool what an update could do: every profile and its state, and the newer
/// builds, or with `all` every build any profile could move to, whatever its state.
/// Blocks for as long as it checks the profiles and reads the build server, which is
/// seconds: call it from a thread.
pub fn list(all: bool) -> Result<Listing, String> {
    let args: &[&str] = if all {
        &["update-profile", "--list", "--every", "--all"]
    } else {
        &["update-profile", "--list", "--every"]
    };
    let out = crate::boot::tool(args)
        .output()
        .map_err(|e| format!("cannot start update-profile: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("update-profile failed")
            .into());
    }
    parse_list(&String::from_utf8_lossy(&out.stdout))
}

/// Update every profile to `build`, reporting on `tx` until it ends with `Done` or
/// `Failed`. `force` updates a profile whose packages or system files changed too,
/// dropping those changes, and takes a build that is not newer; the rest carries as ever.
/// Blocks for the whole update: call it from a thread.
pub fn update(build: u32, force: bool, tx: &Sender<Step>) {
    let number = build.to_string();
    let mut args = vec!["update-profile", "--progress", "--every"];
    if force {
        args.push("--force");
    }
    args.push(&number);
    run(&args, tx);
}

/// Put back the version the booted profile's last update kept, reporting on `tx`.
pub fn rollback(tx: &Sender<Step>) {
    let _ = tx.send(Step::Working(0, "Rolling back".into()));
    run(&["update-profile", "--rollback"], tx);
}

/// Run the tool, turning its progress lines into steps, each named after the profile
/// under way, and ending with what became of each profile. A run that failed on some
/// profile and not others is done all the same, the failure being one of its lines.
fn run(args: &[&str], tx: &Sender<Step>) {
    let mut profile = String::new();
    let mut results = Vec::new();
    let result = crate::app::run_logged(&mut crate::boot::tool(args), &mut |line: String| {
        if let Some((pct, step)) = parse_progress(&line) {
            let step = if profile.is_empty() { step } else { format!("{profile}: {step}") };
            let _ = tx.send(Step::Working(pct, step));
        } else if let Some(name) = line.strip_prefix("@profile ") {
            profile = crate::boot::display_name(name.trim());
        } else if let Some(said) = parse_result(&line) {
            results.push(said);
        }
    });
    let _ = tx.send(match result {
        Err(e) if results.is_empty() => Step::Failed(e),
        _ => Step::Done(results),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_listing() {
        let l = parse_list(
            "profile @Desktop Desktop 1230 booted\nprofile @Router Router 1219 packages\n\
             booted @Desktop\nkept 1201\n\
             build 1235 better-kde 4981 Desktop,Router\nbuild 1231 dev 942021864 Desktop\n",
        )
        .unwrap();
        assert_eq!(l.profiles.len(), 2);
        assert_eq!(
            l.profiles[1],
            Profile {
                name: "@Router".into(),
                kind: "Router".into(),
                build: 1219,
                state: "packages".into()
            }
        );
        assert_eq!(l.booted.as_deref(), Some("@Desktop"));
        assert_eq!(l.kept, Some(1201));
        assert_eq!(l.offers.len(), 2);
        assert_eq!(l.offers[0].kinds, ["Desktop", "Router"]);
        assert_eq!(l.offers[1].bytes, 942021864);
    }

    #[test]
    fn nothing_newer_and_nothing_kept() {
        let l = parse_list("profile @Desktop Desktop 1230 pristine\n").unwrap();
        assert_eq!(l.kept, None);
        assert_eq!(l.booted, None);
        assert!(l.offers.is_empty());
    }

    #[test]
    fn a_modified_profile_goes_only_when_forced() {
        let l = parse_list(
            "profile @Desktop Desktop 1230 customized\nprofile @Router Router 1219 system\n\
             profile @Minimal Minimal 1235 pristine\n\
             build 1235 dev 1 Desktop,Router,Minimal\n",
        )
        .unwrap();
        let names = |force| -> Vec<String> {
            l.going(&l.offers[0], force).iter().map(|p| p.name.clone()).collect()
        };
        assert_eq!(names(false), ["@Desktop"]);
        assert_eq!(names(true), ["@Desktop", "@Router", "@Minimal"]);
    }

    #[test]
    fn reads_result_lines() {
        assert_eq!(
            parse_result("result @Desktop updated 1230 1235").as_deref(),
            Some("Desktop 1230 to 1235")
        );
        assert_eq!(
            parse_result("result @Router blocked packages").as_deref(),
            Some("Router blocked")
        );
        assert_eq!(
            parse_result("result @Desktop__Desktop-clone__ current 1235").as_deref(),
            Some("[Desktop clone] already at 1235")
        );
        assert_eq!(parse_result("result @X unavailable").as_deref(), Some("X not in this build"));
        assert_eq!(parse_result("result @X failed").as_deref(), Some("X failed"));
        assert_eq!(parse_result("@progress 5 Downloading images"), None);
    }

    #[test]
    fn only_a_clean_or_config_changed_profile_is_updatable() {
        assert!(updatable("pristine"));
        assert!(updatable("booted"));
        assert!(updatable("customized"));
        assert!(!updatable("packages"));
        assert!(!updatable("system"));
        assert!(!updatable("error"));
    }

    #[test]
    fn unknown_lines_are_skipped_and_no_profile_is_an_error() {
        assert!(parse_list("something new\nprofile @M Minimal 5 pristine\n").is_ok());
        assert!(parse_list("kept 3\n").is_err());
    }

    #[test]
    fn reads_progress_lines() {
        assert_eq!(parse_progress("@progress 42 Downloading"), Some((42, "Downloading".into())));
        assert_eq!(
            parse_progress("@progress 85 Carrying your changes"),
            Some((85, "Carrying your changes".into()))
        );
        assert_eq!(parse_progress("@progress 250 Done"), Some((100, "Done".into())));
        assert_eq!(parse_progress("fetching x (1.0MB)..."), None);
        assert_eq!(parse_progress("@progress x y"), None);
    }
}
