//! Updating the booted profile to a newer build, through `update-profile`.
//!
//! The tool does the work and owns every decision: which builds suit this profile,
//! what to fetch, how the user's changes are carried and how a failure is undone.
//! This side reads what it says. `update-profile --list` answers in lines a program
//! can read, one fact each:
//!
//! ```text
//! profile @Desktop Desktop 1230
//! kept 1219
//! build 1235 dev 942021864
//! ```
//!
//! Whether the profile may be updated is `check-profile`'s word, asked separately so
//! the two run at once: the list waits on the build server, the check on the disk.
//! An update run with `--progress` adds `@progress <percent> <step>` lines to its
//! output. Only the booted profile is offered: the tool defaults to it, so neither
//! command here names one.

use std::sync::mpsc::Sender;

/// A build the profile could move to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub number: u32,
    /// The build-scripts branch it was built from, which is what a channel is.
    pub branch: String,
    /// What would be downloaded for it, in bytes.
    pub bytes: u64,
}

/// Everything the screen shows before anything is started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// The subvolume, e.g. `@Desktop`.
    pub profile: String,
    /// Which stock it is built from, e.g. `Desktop`.
    pub kind: String,
    pub build: u32,
    /// The build the last update kept, to roll back to.
    pub kept: Option<u32>,
    /// Newest first, as the tool lists them.
    pub offers: Vec<Offer>,
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
    /// Finished; what the tool said it did.
    Done(String),
    Failed(String),
}

/// Read `update-profile --list`'s answer. Lines it does not know are skipped, so the
/// tool can say more than this reads.
pub fn parse_list(text: &str) -> Result<Listing, String> {
    let mut out = Listing::default();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.as_slice() {
            ["profile", name, kind, build] => {
                out.profile = (*name).into();
                out.kind = (*kind).into();
                out.build = build.parse().map_err(|_| format!("bad build number: {build}"))?;
            }
            ["kept", build] => out.kept = build.parse().ok(),
            ["build", number, branch, bytes] => {
                if let (Ok(number), Ok(bytes)) = (number.parse(), bytes.parse()) {
                    out.offers.push(Offer { number, branch: (*branch).into(), bytes });
                }
            }
            _ => {}
        }
    }
    if out.profile.is_empty() {
        return Err("update-profile said nothing about this profile".into());
    }
    Ok(out)
}

/// A progress line, as percent and step, or `None` for any other line.
pub fn parse_progress(line: &str) -> Option<(u8, String)> {
    let rest = line.strip_prefix("@progress ")?;
    let (pct, step) = rest.split_once(' ')?;
    Some((pct.parse::<u8>().ok()?.min(100), step.trim().to_string()))
}

/// Ask the tool what an update could do: the newer builds, or with `all` every build
/// this profile could move to, older ones too. Blocks for as long as it reads the
/// build server, which is seconds: call it from a thread.
pub fn list(all: bool) -> Result<Listing, String> {
    let args: &[&str] =
        if all { &["update-profile", "--list", "--all"] } else { &["update-profile", "--list"] };
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

/// What check-profile says about the booted profile: pristine, booted, customized,
/// packages or system. Seconds at most: call it from a thread.
pub fn check() -> Result<String, String> {
    let out = crate::boot::tool(&["check-profile"])
        .output()
        .map_err(|e| format!("cannot start check-profile: {e}"))?;
    let word = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && !word.is_empty() {
        Ok(word)
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("check-profile failed")
            .into())
    }
}

/// Update the booted profile to `build`, reporting on `tx` until it ends with `Done`
/// or `Failed`. `force` updates a profile whose packages or system files changed too,
/// dropping those changes, and takes a build that is not newer; the rest carries as ever.
/// Blocks for the whole update: call it from a thread.
pub fn update(build: u32, force: bool, tx: &Sender<Step>) {
    let number = build.to_string();
    let mut args = vec!["update-profile", "--progress"];
    if force {
        args.push("--force");
    }
    args.push(&number);
    run(&args, "updated from", tx);
}

/// Put back the version the last update kept, reporting on `tx`.
pub fn rollback(tx: &Sender<Step>) {
    let _ = tx.send(Step::Working(0, "Rolling back".into()));
    run(&["update-profile", "--rollback"], "is back at build", tx);
}

/// Run the tool, turning its progress lines into steps and ending with what it said
/// about `done`, the line its success is reported in.
fn run(args: &[&str], done: &str, tx: &Sender<Step>) {
    let mut said = String::new();
    let result = crate::app::run_logged(&mut crate::boot::tool(args), &mut |line: String| {
        if let Some((pct, step)) = parse_progress(&line) {
            let _ = tx.send(Step::Working(pct, step));
        } else if line.contains(done) {
            said = line;
        }
    });
    let _ = tx.send(match result {
        Ok(()) => Step::Done(said),
        Err(e) => Step::Failed(e),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_listing() {
        let l = parse_list(
            "profile @Router Router 1219\nkept 1201\n\
             build 1230 better-kde 4981\nbuild 1228 dev 942021864\n",
        )
        .unwrap();
        assert_eq!(l.profile, "@Router");
        assert_eq!(l.kind, "Router");
        assert_eq!(l.build, 1219);
        assert_eq!(l.kept, Some(1201));
        assert_eq!(l.offers.len(), 2);
        assert_eq!(l.offers[1], Offer { number: 1228, branch: "dev".into(), bytes: 942021864 });
    }

    #[test]
    fn nothing_newer_and_nothing_kept() {
        let l = parse_list("profile @Desktop Desktop 1230\n").unwrap();
        assert_eq!(l.kept, None);
        assert!(l.offers.is_empty());
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
        assert!(parse_list("something new\nprofile @M Minimal 5\n").is_ok());
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
