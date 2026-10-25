use fspec_core::Scope;
use fspec_core::apply::{apply_scope, plan_scope};
use fspec_core::facts::Init;
use fspec_core::plan::Action;
use fspec_core::state::State;
use serde_json::json;

use super::*;
use crate::testutil::{World, desired, resource};

fn locale(json: serde_json::Value) -> Resource {
    let mut j = json;
    j["kind"] = json!("locale");
    resource(j)
}

fn world(distro: &str, init: Init) -> World {
    let mut w = World::linux(init);
    w.facts.distro = distro.into();
    write_file(&w.paths.system("/usr/share/zoneinfo/Europe/Madrid"), "TZif").unwrap();
    w
}

fn read(w: &World, p: &str) -> String {
    fs::read_to_string(w.paths.system(p)).unwrap()
}

#[test]
fn set_var_replaces_the_key_and_keeps_the_rest() {
    assert_eq!(set_var("FONT=lat2\nKEYMAP=us\n", "KEYMAP", "de"), "FONT=lat2\nKEYMAP=de\n");
    assert_eq!(set_var("", "LANG", "en_US.UTF-8"), "LANG=en_US.UTF-8\n");
    assert_eq!(set_var("#KEYMAP=us\nKEYMAPS=x\n", "KEYMAP", "de"), "#KEYMAP=us\nKEYMAPS=x\nKEYMAP=de\n");
}

#[test]
fn enable_uncomments_the_distro_line_or_appends_it() {
    let gen_file = "#  en_GB.UTF-8 UTF-8\n#en_US.UTF-8 UTF-8\n";
    assert_eq!(enable(gen_file, "en_US.UTF-8 UTF-8"), "#  en_GB.UTF-8 UTF-8\nen_US.UTF-8 UTF-8\n");
    assert_eq!(enable("", "de_DE.UTF-8 UTF-8"), "de_DE.UTF-8 UTF-8\n");
}

#[test]
fn check_refuses_names_that_are_not_names() {
    let w = world("arch", Init::Systemd);
    let cx = w.cx(Scope::System);
    assert!(LocaleBackend.check(&cx, &locale(json!({"locale": "en_US.UTF-8", "keymap": "de-latin1"}))).is_ok());
    assert!(LocaleBackend.check(&cx, &locale(json!({"locale": "en_US"}))).is_err());
    assert!(LocaleBackend.check(&cx, &locale(json!({"keymap": "us; rm"}))).is_err());
    assert!(LocaleBackend.check(&cx, &locale(json!({"timezone": "../etc/passwd"}))).is_err());
    assert!(LocaleBackend.check(&cx, &locale(json!({"timezone": "America/Argentina/Buenos_Aires"}))).is_ok());
    let mac = World::darwin();
    assert!(LocaleBackend.check(&mac.cx(Scope::System), &locale(json!({"timezone": "Europe/Madrid"}))).is_err());
}

#[test]
fn arch_writes_the_files_and_generates_the_locale_once() {
    let w = world("arch", Init::Systemd);
    write_file(&w.paths.system("/etc/locale.gen"), "#en_US.UTF-8 UTF-8\n").unwrap();
    let backends: &[&dyn Backend] = &[&LocaleBackend];
    let want = vec![desired(locale(json!({"locale": "en_US.UTF-8", "keymap": "us", "timezone": "Europe/Madrid"})))];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    assert_eq!(read(&w, "/etc/locale.conf"), "LANG=en_US.UTF-8\n");
    assert_eq!(read(&w, "/etc/vconsole.conf"), "KEYMAP=us\nXKBLAYOUT=us\n");
    assert_eq!(read(&w, "/etc/locale.gen"), "en_US.UTF-8 UTF-8\n");
    let link = fs::read_link(w.paths.system(LOCALTIME)).unwrap();
    assert_eq!(link, Path::new("../usr/share/zoneinfo/Europe/Madrid"));
    assert_eq!(w.runner.lines(), ["locale-gen"]);

    let state = State::load(&fspec_core::apply::state_dir(&w.paths, Scope::System).join("state.json")).unwrap();
    let plan = plan_scope(&w.cx(Scope::System), backends, &want, &state).unwrap();
    assert!(plan.changes.is_empty(), "{plan}");

    fs::write(w.paths.system("/etc/vconsole.conf"), "KEYMAP=fr\n").unwrap();
    let plan = plan_scope(&w.cx(Scope::System), backends, &want, &state).unwrap();
    assert!(matches!(&plan.changes[0].action, Action::Drift { .. }), "{plan}");
}

#[test]
fn void_keeps_its_keymap_in_rc_conf_and_reconfigures_glibc_locales() {
    let w = world("void", Init::Runit);
    write_file(&w.paths.system("/etc/rc.conf"), "HOSTNAME=lab\n#KEYMAP=\"es\"\n").unwrap();
    let want = vec![desired(locale(json!({"locale": "de_DE.UTF-8", "keymap": "de"})))];
    apply_scope(&w.cx(Scope::System), &[&LocaleBackend], &want).unwrap();
    assert_eq!(read(&w, "/etc/rc.conf"), "HOSTNAME=lab\n#KEYMAP=\"es\"\nKEYMAP=de\n");
    assert_eq!(read(&w, "/etc/default/libc-locales"), "de_DE.UTF-8 UTF-8\n");
    assert_eq!(read(&w, "/etc/locale.conf"), "LANG=de_DE.UTF-8\n");
    assert_eq!(w.runner.lines(), ["xbps-reconfigure -f glibc-locales"]);
}

#[test]
fn debian_and_fedora_take_their_own_files_and_tools() {
    let w = world("debian", Init::Systemd);
    let want = vec![desired(locale(json!({"locale": "es_ES.UTF-8", "keymap": "es", "timezone": "Europe/Madrid"})))];
    apply_scope(&w.cx(Scope::System), &[&LocaleBackend], &want).unwrap();
    assert_eq!(read(&w, "/etc/default/locale"), "LANG=es_ES.UTF-8\n");
    assert_eq!(read(&w, "/etc/default/keyboard"), "XKBLAYOUT=es\n");
    assert_eq!(read(&w, "/etc/timezone"), "Europe/Madrid\n");
    assert_eq!(w.runner.lines(), ["locale-gen"]);

    let w = world("fedora", Init::Systemd);
    let want = vec![desired(locale(json!({"locale": "es_ES.UTF-8"})))];
    apply_scope(&w.cx(Scope::System), &[&LocaleBackend], &want).unwrap();
    assert_eq!(w.runner.lines(), ["dnf install -y glibc-langpack-es"]);
}

#[test]
fn a_missing_zone_fails_and_leaves_localtime_alone() {
    let w = world("arch", Init::Systemd);
    let want = vec![desired(locale(json!({"timezone": "Mars/Olympus"})))];
    let err = apply_scope(&w.cx(Scope::System), &[&LocaleBackend], &want).unwrap_err();
    assert!(format!("{err:#}").contains("Mars/Olympus"), "{err:#}");
    assert!(fs::symlink_metadata(w.paths.system(LOCALTIME)).is_err());
}
