//! Boot listing and boot entry parsing.
//!
//! The listing is positional and one column only sometimes exists, which is
//! exactly the shape that goes wrong silently, so it is pinned here against real
//! output from the device. The entries are pinned against real files for the same
//! reason: which kernel a row boots is decided by what is read out of them.

use flipper_ui::boot;

/// `list-profiles` output as the device prints it, booted marker and all.
const LISTING: &str = "\
Currently booted profile: @Minimal (id 263)

NAME                      KIND     ID   CREATED              LAST USED  RO  PARENT                         ORIGIN
@Desktop                  profile  265  2026-08-20 10:36:19  never      rw  @Desktop_968_stock (264)       @Desktop_968_stock (264)
@Minimal       <- booted  profile  263  2026-08-20 10:34:01  now        rw  @Minimal_968_stock (262)       @Minimal_968_stock (262)
@Desktop__My-Games__      profile  280  2026-08-19 09:00:00  2026-08-19 12:00:00  rw  @Desktop (265)  @Desktop_968_stock (264)
@Minimal_old_1            old      299  2026-08-01 00:00:00  never      ro  -                              -
";

/// The booted marker occupies its own column, shifting every field after it.
///
/// Parsing at fixed offsets reads the booted row's KIND as its ID and drops it,
/// so the profile you are running would be missing from the list.
#[test]
fn the_booted_marker_does_not_shift_the_other_columns() {
    let rows = boot::parse_listing(LISTING, boot::Medium::Internal, "", "/dev/sda", "UFS");
    let names: Vec<&str> = rows.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        ["@Desktop", "@Minimal", "@Desktop__My-Games__"],
        "the _old row is a leftover, not somewhere to boot"
    );

    let minimal = rows.iter().find(|p| p.name == "@Minimal").unwrap();
    assert!(minimal.booted);
    assert_eq!(minimal.id, "263");
    assert_eq!(minimal.last_used, "now");
    assert_eq!(minimal.origin, "@Minimal_968_stock", "the id is stripped");

    let desktop = rows.iter().find(|p| p.name == "@Desktop").unwrap();
    assert!(!desktop.booted);
    assert_eq!(desktop.id, "265");
    assert_eq!(desktop.last_used, "never");
}

/// A user profile shows its label in brackets; a factory one shows its name.
#[test]
fn a_derived_profile_is_named_by_its_label() {
    assert_eq!(boot::display_name("@Minimal"), "Minimal");
    assert_eq!(boot::display_name("@Desktop__My-Games__"), "[My Games]");
    // A dash is a space in every name, not only inside a label.
    assert_eq!(boot::display_name("@No-Graphics"), "No Graphics");
}

/// The icon follows the origin's base name, not the profile's own.
///
/// A clone called "[My Games]" carries no hint of what it came from, so keying
/// off its own name would give every clone the fallback icon.
#[test]
fn the_icon_follows_the_origin() {
    assert_eq!(boot::icon_key("@Desktop__My-Games__", "@Desktop_968_stock"), "desktop");
    assert_eq!(boot::icon_key("@Minimal", "@Minimal_968_stock"), "minimal");
    assert_eq!(boot::icon_key("@TV-Media-Box", "@TV-Media-Box_968_stock"), "media");
    assert_eq!(boot::icon_key("@Router", "@Router_968_stock"), "router");
    assert_eq!(boot::icon_key("@No-Graphics", "@No-Graphics_968_stock"), "graphics");
    // No origin at all: fall back to the name.
    assert_eq!(boot::icon_key("@Router", ""), "router");
    assert_eq!(boot::icon_key("@Whatever", ""), "");
    assert_eq!(boot::origin_base("@Desktop_968_stock"), "Desktop");
    assert_eq!(boot::origin_base("@Desktop"), "", "not a stock name");
}

/// The two sentinels, then relative time in singular units.
#[test]
fn used_ago_reads_as_a_person_would_say_it() {
    let at = |s: &str| {
        std::time::UNIX_EPOCH
            + std::time::Duration::from_secs(
                boot::parse_stamp_for_test(s).expect("parseable") as u64,
            )
    };
    let now = at("2026-08-20 12:00:00");

    assert_eq!(boot::used_ago("never", now), "");
    assert_eq!(boot::used_ago("", now), "");
    assert_eq!(boot::used_ago("now", now), "Running");
    assert_eq!(boot::used_ago("2026-08-20 11:59:30", now), "Used just now");
    assert_eq!(boot::used_ago("2026-08-20 11:59:00", now), "Used 1 min ago");
    assert_eq!(boot::used_ago("2026-08-20 11:00:00", now), "Used 1 hour ago");
    assert_eq!(boot::used_ago("2026-08-20 09:00:00", now), "Used 3 hours ago");
    assert_eq!(boot::used_ago("2026-08-18 12:00:00", now), "Used 2 days ago");
    assert_eq!(boot::used_ago("2026-06-01 12:00:00", now), "Used 2 months ago");
    // Not a timestamp: shown verbatim rather than turned into a wrong duration.
    assert_eq!(boot::used_ago("yesterday", now), "yesterday");
}

/// A factory profile cannot be renamed or deleted, and the booted one cannot be
/// deleted at all.
///
/// Offering an action the tool refuses is worse than not offering it: the user
/// presses it, waits, and gets an error that was predictable.
#[test]
fn the_actions_offered_depend_on_the_profile() {
    let factory = boot::Profile {
        name: "@Minimal".into(),
        origin: "@Minimal_968_stock".into(),
        ..Default::default()
    };
    assert_eq!(
        boot::edit_actions(&factory),
        ["Clone", "Factory Reset", "Auto Start"],
        "a factory profile's name ties it to its stock"
    );

    let user = boot::Profile {
        name: "@Desktop__My-Games__".into(),
        origin: "@Desktop_968_stock".into(),
        ..Default::default()
    };
    assert_eq!(
        boot::edit_actions(&user),
        ["Rename", "Clone", "Factory Reset", "Delete", "Auto Start"]
    );

    let booted = boot::Profile { booted: true, ..user.clone() };
    assert!(
        !boot::edit_actions(&booted).contains(&"Delete"),
        "delete-profile refuses the running profile"
    );
}

/// A clone gets a fresh name, and cloning twice does not collide.
#[test]
fn a_clone_is_named_from_its_source() {
    let src = boot::Profile {
        name: "@Minimal".into(),
        origin: "@Minimal_968_stock".into(),
        ..Default::default()
    };
    let existing = vec![src.clone()];
    let first = boot::clone_dest(&src, &existing);
    assert_eq!(first, "@Minimal__Minimal-clone__");

    // With that one taken, the next gets a number rather than failing.
    let mut existing = existing;
    existing.push(boot::Profile { name: first.clone(), ..Default::default() });
    assert_eq!(boot::clone_dest(&src, &existing), "@Minimal__Minimal-clone-2__");

    // A clone of a clone reads from its label, not the whole subvolume name.
    let clone = boot::Profile {
        name: "@Desktop__My-Games__".into(),
        origin: "@Desktop_968_stock".into(),
        ..Default::default()
    };
    assert_eq!(boot::clone_dest(&clone, &[]), "@Desktop__My-Games-clone__");
}

/// Names that cannot be a profile are refused before a tool is asked to act.
#[test]
fn actions_refuse_a_name_that_cannot_be_a_profile() {
    for bad in ["", "@", "Minimal", "@has space", "@semi;colon", "@../escape"] {
        assert!(
            boot::clone("", bad, "@Ok__x__").is_err(),
            "{bad:?} should be refused as a source"
        );
        assert!(
            boot::clone("", "@Minimal", bad).is_err(),
            "{bad:?} should be refused as a destination"
        );
    }
    // The marker names an entry file, so a name is what it takes and a path is not:
    // nothing here builds a path out of one, and a value with a separator in it cannot
    // be an entry.
    for bad in ["", ".hidden", "has space", "../escape", "sub/dir", "semi;colon"] {
        assert!(
            boot::set_auto_start("", bad).is_err(),
            "{bad:?} should be refused as an entry id"
        );
    }
    assert!(boot::valid_entry_id("900-flipperos-Desktop-7.2.0-00249-g26619ffca0bd"));
}

/// A real entry, as kernel-install writes it for a profile on this device.
const ENTRY: &str = "\
# Boot Loader Specification type#1 entry (Flipper One)
title      Desktop 7.2.0-00249-g26619
version    7.2.0-00249-g26619ffca0bd
sort-key   debian
options    root=UUID=b34a8456 audit=0 console=tty1 rootflags=subvol=@Desktop
linux      /@Desktop/usr/lib/modules/7.2.0-00249-g26619ffca0bd/vmlinuz
devicetreedir /@Desktop/usr/lib/linux-image-7.2.0-00249-g26619ffca0bd
initrd     /@Desktop/usr/lib/modules/7.2.0-00249-g26619ffca0bd/initrd
";

/// What a row is: the profile its command line mounts, and the kernel it names.
#[test]
fn an_entry_names_the_subvolume_and_the_kernel_it_boots() {
    let conf = boot::parse_conf("900-flipperos-Desktop-7.2.0-00249-g26619ffca0bd", ENTRY)
        .expect("an entry that mounts a subvolume");
    assert_eq!(conf.subvol, "@Desktop");
    assert_eq!(conf.version, "7.2.0-00249-g26619ffca0bd");
    // The title's version is what a row shows, kernel-install having already trimmed
    // it to fit a menu.
    assert_eq!(conf.short, "7.2.0-00249-g26619");

    // devicetreedir must not be read as devicetree, and neither is an overlay.
    assert!(conf.system.is_empty() && conf.user.is_empty());

    // An entry that mounts nothing is not a row: the loader directory holds whatever
    // anyone has put there.
    assert!(boot::parse_conf("stray", "title Nothing\nlinux /vmlinuz\n").is_none());
}

/// The kernel release, for the entries that state none.
#[test]
fn a_version_comes_from_the_kernel_path_when_the_entry_states_none() {
    let older = "\
options root=UUID=x rootflags=subvol=@Minimal
linux /@Minimal/usr/lib/modules/6.1.172/vmlinuz
";
    let conf = boot::parse_conf("600-flipperos-Minimal-6.1.172", older).unwrap();
    assert_eq!(conf.version, "6.1.172");
    // No title to trim: the release itself is what the row would show.
    assert_eq!(conf.short, "6.1.172");

    // An image that keeps its kernels in /boot names the release in the file.
    let in_boot = "\
options rootflags=subvol=@Minimal
linux /boot/vmlinuz-7.2.0-ga0d2d145deeb
";
    assert_eq!(
        boot::parse_conf("x", in_boot).unwrap().version,
        "7.2.0-ga0d2d145deeb"
    );
}

/// Overlays are the entry's own, and a drop-in is the user's.
///
/// Read from the entry rather than from a directory: a profile's overlay files live
/// inside its subvolume, which is not mounted unless it is the one running, so a
/// directory walk would report none for every profile but the booted one.
#[test]
fn overlays_are_read_from_the_entry_and_split_by_path() {
    let text = "\
options root=UUID=x rootflags=subvol=@No-Graphics
linux /@No-Graphics/usr/lib/modules/7.2.0/vmlinuz
devicetree-overlay /@No-Graphics/usr/lib/rk3576-no-graphics.dtbo /@No-Graphics/etc/kernel/dtbo/mine.dtbo
";
    let conf = boot::parse_conf("500-flipperos-No-Graphics-7.2.0", text).unwrap();
    assert_eq!(conf.system, ["rk3576-no-graphics.dtbo"]);
    assert_eq!(
        conf.user,
        ["mine.dtbo"],
        "a drop-in under /etc/kernel/dtbo is the user's"
    );
}

/// Old kernels are hidden, and an unreadable version is not.
///
/// The releases are git-describe strings, so only the numbers before the first dash
/// compare: that is enough to tell a BSP kernel from a mainline one, which is the
/// question the menu asks. A release that will not parse is shown, because a hidden
/// row cannot be booted and showing one kernel too many is the safer mistake.
#[test]
fn only_the_kernels_worth_choosing_are_offered() {
    for old in ["6.1.172", "6.16.0-rc1", "0.1"] {
        assert!(!boot::version_at_least(old, (7, 0)), "{old} is older than 7.0");
    }
    for new in [
        "7.2.0-00249-g26619ffca0bd",
        "7.2.0-ga0d2d145deeb",
        "7.0",
        "8.1.0",
    ] {
        assert!(boot::version_at_least(new, (7, 0)), "{new} is 7.0 or newer");
    }
    for unreadable in ["", "mainline", "v7.2.0"] {
        assert!(
            boot::version_at_least(unreadable, (7, 0)),
            "{unreadable:?} does not parse, so it is shown"
        );
    }
}

/// The per-subvolume size output is KEY=value pairs, and -q omits REFERENCED.
#[test]
fn size_output_is_parsed_from_key_value_pairs() {
    let quick = boot::parse_space("TOTAL=3.6GB UNIQUE=1.1GB\n").unwrap();
    assert_eq!(quick.total, "3.6GB");
    assert_eq!(quick.unique, "1.1GB");
    assert_eq!(quick.referenced, "", "-q skips the compsize walk");

    let full = boot::parse_space("TOTAL=2.5GB UNIQUE=0.0B REFERENCED=1.4GB COMPRESSION=1.8\n")
        .unwrap();
    assert_eq!(full.referenced, "1.4GB");

    // The whole-filesystem report has no such pairs, so it must not be mistaken
    // for an answer.
    assert!(boot::parse_space("== subvolumes ==\n@Minimal 1.1GB 2.0GB 3.6GB\n").is_none());
    assert!(boot::parse_space("Error: No such subvolume: @Nope\n").is_none());
}

/// Rename and Delete are only offered on a profile a user made, which is the one
/// the list shows in brackets.
///
/// The real listing is the fixture, because this is exactly the case that was
/// wrong on the device: an image whose name does not match its origin base is
/// still an image, not something to rename or delete.
#[test]
fn only_a_user_profile_may_be_renamed_or_deleted() {
    let listing = "\
NAME                                        KIND     ID   CREATED              LAST USED  RO  PARENT                                ORIGIN
@Desktop_Computer                           profile  276  2026-08-20 14:34:48  never      rw  @Desktop_968_stock (264)              @Desktop_968_stock (264)
@Minimal                         <- booted  profile  263  2026-08-20 10:34:01  now        rw  @Minimal_968_stock (262)              @Minimal_968_stock (262)
@TV-Media-Box__TV-Media-Box-clone__         profile  273  2026-08-20 13:40:01  never      rw  @TV-Media-Box (267)                   @TV-Media-Box_968_stock (266)
";
    let profiles = boot::parse_listing(listing, boot::Medium::Internal, "", "/dev/sda", "UFS");
    let by = |name: &str| {
        profiles.iter().find(|p| p.name == name).expect("profile in the listing").clone()
    };

    // A factory image: its name is its origin base.
    assert_eq!(
        boot::edit_actions(&by("@Minimal")),
        ["Clone", "Factory Reset", "Auto Start"]
    );
    // An image made outside this menu. No brackets in the list, so no Rename and
    // no Delete either, even though its name differs from the base.
    assert_eq!(boot::display_name("@Desktop_Computer"), "Desktop_Computer");
    assert_eq!(boot::display_name("@TV-Media-Box"), "TV Media Box");
    assert_eq!(
        boot::edit_actions(&by("@Desktop_Computer")),
        ["Clone", "Factory Reset", "Auto Start"]
    );
    // A user profile, shown in brackets, gets everything.
    let user = by("@TV-Media-Box__TV-Media-Box-clone__");
    assert_eq!(boot::display_name(&user.name), "[TV Media Box clone]");
    assert_eq!(
        boot::edit_actions(&user),
        ["Rename", "Clone", "Factory Reset", "Delete", "Auto Start"]
    );
}

/// The Rename field is seeded with the label, and what it produces is a
/// `@Base__label__` name built from the origin.
#[test]
fn a_rename_builds_a_name_from_the_origin_base() {
    let listing = "\
NAME                                   KIND     ID   CREATED              LAST USED  RO  PARENT                     ORIGIN
@TV-Media-Box__movie-night__           profile  273  2026-08-20 13:40:01  never      rw  @TV-Media-Box (267)        @TV-Media-Box_968_stock (266)
";
    let p = boot::parse_listing(listing, boot::Medium::Internal, "", "/dev/sda", "UFS").remove(0);
    assert_eq!(boot::profile_label(&p.name), "movie night");
    assert_eq!(
        boot::rename_dest(&p, "Movie Night 2"),
        "@TV-Media-Box__Movie-Night-2__"
    );
    // Punctuation is not part of a subvolume name, and the edges are trimmed.
    assert_eq!(boot::encode_label("  hello, world!  "), "hello-world");
    assert_eq!(boot::encode_label("***"), "");
    // Nothing usable in the text means no destination, which is what refuses the
    // commit.
    assert_eq!(boot::rename_dest(&p, "  "), "");
}

/// A card's listing, as `list-profiles -d` prints it.
///
/// The preamble line before the header has to survive: the booted listing does not
/// have one. The ids here are the ones the card in the test device actually carries,
/// and they overlap the internal storage's, which is why a card's row can never wear
/// the heart: nothing in a listing says which filesystem issued an id.
#[test]
fn a_cards_listing_is_tagged_and_never_marked() {
    let listing = "\
Listing /dev/mmcblk0p3, which is not the filesystem you booted from

NAME             KIND     ID   CREATED              LAST USED            RO  PARENT                         ORIGIN
@Desktop         profile  265  2026-08-20 08:44:06  2026-08-26 10:41:34  rw  @Desktop_966_stock (264)       @Desktop_966_stock (264)
@Minimal         profile  263  2026-08-20 08:41:48  never                rw  @Minimal_966_stock (262)       @Minimal_966_stock (262)
";

    // What flipctl passes for a card: no marker, and removable.
    let rows = boot::parse_listing(listing, boot::Medium::Sd, "/dev/mmcblk0p3", "/dev/mmcblk0", "SD");
    assert_eq!(rows.len(), 2, "the preamble must not be read as a row");
    assert!(rows.iter().all(|p| p.medium == boot::Medium::Sd), "every row came off the card");
    assert_eq!(rows[0].name, "@Desktop");
    assert_eq!(rows[0].id, "265");
    assert!(!rows[0].booted, "the card's profile is not what is running");

    // The same ids on the internal storage: the tag is what tells the two apart, not
    // the id, which is why a card's row can never wear the heart.
    let own = boot::parse_listing(listing, boot::Medium::Internal, "", "/dev/sda", "UFS");
    assert!(own[0].medium == boot::Medium::Internal);
}

/// Auto Start is not offered for a card's profile, and everything else still is.
#[test]
fn a_cards_profile_cannot_be_marked() {
    let mut p = boot::Profile { name: "@Minimal".into(), ..Default::default() };
    assert!(boot::edit_actions(&p).contains(&"Auto Start"));
    p.medium = boot::Medium::Sd;
    let actions = boot::edit_actions(&p);
    assert!(!actions.contains(&"Auto Start"), "got {actions:?}");
    assert!(actions.contains(&"Clone"), "the rest are unaffected: {actions:?}");
}

/// A card's rows carry the device that tells them apart from the internal storage's
/// profiles of the same name.
#[test]
fn a_cards_rows_carry_their_device() {
    let listing = "\
NAME             KIND     ID   CREATED              LAST USED  RO  PARENT  ORIGIN
@Desktop         profile  265  2026-08-20 08:44:06  never      rw  -       -
";
    let card = boot::parse_listing(listing, boot::Medium::Sd, "/dev/mmcblk0p3", "/dev/mmcblk0", "SD");
    assert_eq!(card[0].dev, "/dev/mmcblk0p3");
    // What the Info popup's Drive line reads from.
    assert_eq!(card[0].disk, "/dev/mmcblk0");
    assert_eq!(card[0].kind, "SD");
    // The booted filesystem is every tool's default, so its rows name no device.
    let own = boot::parse_listing(listing, boot::Medium::Internal, "", "/dev/sda", "UFS");
    assert!(own[0].dev.is_empty());
}


/// What counts as a leftover from a factory reset, and what does not.
///
/// The answer decides what gets deleted, so the stamp's shape is checked rather than
/// assumed: `@Desktop_old_notes` is somebody's own subvolume.
#[test]
fn only_stamped_copies_of_the_booted_profile_are_leftovers() {
    assert!(boot::is_old_backup("@Desktop_old_2026-08-27_14-31-05", "@Desktop"));
    // create-profile adds a counter when two land in the same second.
    assert!(boot::is_old_backup("@Desktop_old_2026-08-27_14-31-05_2", "@Desktop"));

    assert!(!boot::is_old_backup("@Desktop_old_notes", "@Desktop"), "not a stamp");
    assert!(!boot::is_old_backup("@Desktop_old_2026-8-27_14-31-05", "@Desktop"), "short month");
    assert!(!boot::is_old_backup("@Desktop_old_2026-08-27_14-31-05_x", "@Desktop"), "counter is digits");
    assert!(!boot::is_old_backup("@Desktop", "@Desktop"), "the profile itself");
    assert!(
        !boot::is_old_backup("@Minimal_old_2026-08-27_14-31-05", "@Desktop"),
        "another profile's leftover may be someone's way back"
    );
}

/// An entry file name carries boot-counting state, and the id survives it.
///
/// The counter moves under us -- every attempt renames the file -- so anything that
/// remembers an entry has to remember the id, and anything that reads one has to take
/// the counter off the name first.
#[test]
fn a_boot_counter_is_part_of_the_name_and_not_of_the_id() {
    let id = "900-flipperos-Desktop-7.2.0-00249-g26619ffca0bd";
    assert_eq!(
        boot::split_counter(&format!("{id}+3-0.conf")),
        (id.to_string(), Some(3)),
        "freshly armed: three tries, none spent"
    );
    assert_eq!(boot::split_counter(&format!("{id}+1-2.conf")), (id.to_string(), Some(1)));
    assert_eq!(
        boot::split_counter(&format!("{id}+0-3.conf")),
        (id.to_string(), Some(0)),
        "spent every try, which is what 'bad' means"
    );
    assert_eq!(
        boot::split_counter(&format!("{id}.conf")),
        (id.to_string(), None),
        "no counter at all: a good boot blessed it"
    );

    // A name nobody here wrote is not a reason to call an entry bad and refuse it.
    assert_eq!(
        boot::split_counter("weird+notanumber.conf"),
        ("weird".to_string(), None)
    );
}

/// What an entry says about itself, which the Config screen shows beside its version.
#[test]
fn an_entry_reports_whether_its_boot_is_proven() {
    let armed = boot::Entry { tries: Some(3), ..Default::default() };
    let tried = boot::Entry { tries: Some(1), ..Default::default() };
    let spent = boot::Entry { tries: Some(0), ..Default::default() };
    let blessed = boot::Entry { tries: None, ..Default::default() };

    assert_eq!(armed.state(), Some("untried"));
    assert_eq!(tried.state(), Some("untried"));
    assert_eq!(spent.state(), Some("failed"));
    assert_eq!(
        blessed.state(),
        None,
        "no counter says nothing: blessed and never-counted are the same file"
    );

    assert!(spent.bad(), "an entry with no tries left sorts last");
    assert!(!armed.bad() && !tried.bad() && !blessed.bad());
}

/// The boot order, which is the whole of the menu's state.
///
/// The same four rules `libs/flipper-blsname.sh` sorts by in shell, so the menu and the
/// tools always agree about what boots. Pinned here because the failure is invisible:
/// every entry still boots, just not the one anybody chose.
#[test]
fn the_first_entry_is_the_one_that_boots() {
    let conf = |file: &str, version: &str, key: &str, at: u64, tries: Option<u32>| boot::Conf {
        id: boot::split_counter(file).0,
        version: version.to_string(),
        file: file.to_string(),
        key: key.to_string(),
        tries,
        at,
        ..Default::default()
    };

    // The device as it stands: @Desktop boots by itself (autoboot digit 0) and has two
    // kernels, the chosen one at rank 0; the other profiles follow by band.
    let desktop_new = conf("900-flipperos-Desktop-7.2.0-00249+3-0.conf", "7.2.0-00249", "debian-0100-Desktop-0", 200, Some(3));
    let desktop_old = conf("900-flipperos-Desktop-7.2.0-ga0d2.conf", "7.2.0-ga0d2", "debian-0100-Desktop-1", 100, None);
    let tv = conf("800-flipperos-TV-Media-Box-7.2.0-ga0d2.conf", "7.2.0-ga0d2", "debian-1200-TV-Media-Box-0", 100, None);
    let minimal = conf("600-flipperos-Minimal-7.2.0-ga0d2.conf", "7.2.0-ga0d2", "debian-1400-Minimal-0", 100, None);

    let mut order: Vec<&boot::Conf> = vec![&minimal, &tv, &desktop_old, &desktop_new];
    boot::sort_confs(&mut order);
    assert_eq!(
        order.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(),
        [
            "debian-0100-Desktop-0",
            "debian-0100-Desktop-1",
            "debian-1200-TV-Media-Box-0",
            "debian-1400-Minimal-0"
        ],
        "the autoboot digit leads, then the band, then the rank"
    );

    // Every try spent: the entry sorts last however good its key, and the kernel that
    // was booting before leads again. This is the fallback, and it needs no state.
    let failed = conf("900-flipperos-Desktop-7.2.0-00249+0-3.conf", "7.2.0-00249", "debian-0100-Desktop-0", 200, Some(0));
    let mut order: Vec<&boot::Conf> = vec![&failed, &desktop_old, &tv];
    boot::sort_confs(&mut order);
    assert_eq!(
        order.first().map(|c| c.file.as_str()),
        Some("900-flipperos-Desktop-7.2.0-ga0d2.conf"),
        "a kernel that will not boot hands the device back"
    );
    assert_eq!(order.last().map(|c| c.tries), Some(Some(0)));

    // A higher version leads, whenever the version says anything: an old kernel rebuilt
    // today must not outrank a new one just for being newer on disk.
    let old_rebuilt = conf("900-flipperos-Desktop-6.1.172.conf", "6.1.172", "debian-0100-Desktop-1", 900, None);
    let mut order: Vec<&boot::Conf> = vec![&old_rebuilt, &desktop_old];
    boot::sort_confs(&mut order);
    assert_eq!(
        order.first().map(|c| c.file.as_str()),
        Some("900-flipperos-Desktop-7.2.0-ga0d2.conf"),
        "7.2.0 leads 6.1.172 whatever the file dates say"
    );
    assert_eq!(boot::version_rank("7.10.0") > boot::version_rank("7.9.0"), true);
    assert_eq!(boot::version_rank("mainline"), (0, 0, 0));

    // Two entries a build wrote in the same second, with keys that cannot separate them
    // either: newest first, then the name descending, and never an arbitrary answer.
    let a = conf("900-flipperos-Desktop-7.2.0-a.conf", "7.2.0-a", "debian-0100-Desktop-1", 100, None);
    let b = conf("900-flipperos-Desktop-7.2.0-b.conf", "7.2.0-b", "debian-0100-Desktop-1", 100, None);
    let newer = conf("900-flipperos-Desktop-7.2.0-c.conf", "7.2.0-c", "debian-0100-Desktop-1", 500, None);
    let mut order: Vec<&boot::Conf> = vec![&a, &b, &newer];
    boot::sort_confs(&mut order);
    assert_eq!(
        order.iter().map(|c| c.file.as_str()).collect::<Vec<_>>(),
        [
            "900-flipperos-Desktop-7.2.0-c.conf",
            "900-flipperos-Desktop-7.2.0-b.conf",
            "900-flipperos-Desktop-7.2.0-a.conf"
        ]
    );
}

/// The sort-key is read off the entry, since that is where the order lives.
#[test]
fn an_entry_carries_the_key_that_orders_it() {
    let text = "\
title      Desktop 7.2.0-00249-g26619
version    7.2.0-00249-g26619ffca0bd
sort-key   debian-0100-Desktop-0
options    root=UUID=x rootflags=subvol=@Desktop flipper.entry=900-flipperos-Desktop-7.2.0-00249-g26619ffca0bd
linux      /@Desktop/usr/lib/modules/7.2.0-00249-g26619ffca0bd/vmlinuz
";
    let conf = boot::parse_conf("900-flipperos-Desktop-7.2.0-00249-g26619ffca0bd+3-0.conf", text)
        .expect("an entry that mounts a subvolume");
    assert_eq!(conf.key, "debian-0100-Desktop-0");
    assert_eq!(conf.tries, Some(3));
    assert_eq!(conf.id, "900-flipperos-Desktop-7.2.0-00249-g26619ffca0bd");
    assert_eq!(conf.file, "900-flipperos-Desktop-7.2.0-00249-g26619ffca0bd+3-0.conf");
    assert_eq!(conf.subvol, "@Desktop");
    assert_eq!(conf.short, "7.2.0-00249-g26619");
}
