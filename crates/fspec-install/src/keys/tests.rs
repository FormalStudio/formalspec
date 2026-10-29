use fspec_core::runner::{Output, RecordingRunner};

use super::*;

const SSH_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACDoh10A29aO/vlbh1q5CkfgtibH0F0YO1w8VMWX3bNXdwAAAJDlQWwQ5UFs
EAAAAAtzc2gtZWQyNTUxOQAAACDoh10A29aO/vlbh1q5CkfgtibH0F0YO1w8VMWX3bNXdw
AAAEAYymhLTGAcxGs16xzJ7kCRcERnBnaXJs9H4fWdI+69oeiHXQDb1o7++VuHWrkKR+C2
JsfQXRg7XDxUxZfds1d3AAAACmZzcGVjLXRlc3QBAgM=
-----END OPENSSH PRIVATE KEY-----
";
const SSH_PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOiHXQDb1o7++VuHWrkKR+C2JsfQXRg7XDxUxZfds1d3";

fn age_key() -> (String, String) {
    let id = age::x25519::Identity::generate();
    (id.to_string().expose_secret().to_string(), id.to_public().to_string())
}

#[test]
fn reads_age_identity_files_and_ssh_keys_with_their_public_halves() {
    let (secret, public) = age_key();
    let key = Key::parse(format!("# created: today\n# public key: {public}\n{secret}\n").as_bytes()).unwrap();
    assert_eq!((key.kind, key.public.clone(), key.generated), (Kind::Age, vec![public.clone()], false));
    assert!(key.label().starts_with(&format!("age key {}", &public[..10])));
    assert_eq!(Key::parse(format!("  {secret}").as_bytes()).unwrap().public, [public]);

    let ssh = Key::parse(SSH_KEY.as_bytes()).unwrap();
    assert_eq!((ssh.kind, ssh.public.clone()), (Kind::Ed25519, vec![SSH_PUB.to_string()]));
    assert_eq!(ssh.private.0, SSH_KEY);

    for bad in ["hunter22", "AGE-SECRET-KEY-1NOTAKEY", "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n"] {
        assert!(Key::parse(bad.as_bytes()).is_err(), "{bad}");
    }
    let made = Key::generate();
    assert!(made.generated && made.private.0.contains(&format!("# public key: {}", made.public[0])));
    assert_eq!(Key::parse(made.private.0.as_bytes()).unwrap().public, made.public);
}

#[test]
fn shows_which_secrets_a_key_opens() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(coverage(dir.path(), None).unwrap(), None);
    let key = Key::generate();
    let other = age_key().1;
    std::fs::write(
        dir.path().join("secrets.star"),
        format!(
            "me = \"{}\"\nsecrets = {{\"secrets/a.age\": [me], \"secrets/b.age\": [\"{other}\"], \"secrets/c.age\": [me]}}\n",
            key.public[0]
        ),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("secrets")).unwrap();
    let put = |name: &str, to: &str| {
        let bytes = fspec_files::secret::encrypt(b"x", &[to.to_string()]).unwrap();
        std::fs::write(dir.path().join(name), bytes).unwrap();
    };
    put("secrets/a.age", &key.public[0]);
    put("secrets/b.age", &other);
    let opens = coverage(dir.path(), Some(&key)).unwrap().unwrap();
    assert_eq!(opens, [("secrets/a.age".to_string(), true), ("secrets/b.age".to_string(), false)]);
    let none = coverage(dir.path(), Some(&Key::parse(SSH_KEY.as_bytes()).unwrap())).unwrap().unwrap();
    assert!(none.iter().all(|(_, opens)| !opens));
    assert!(coverage(dir.path(), None).unwrap().unwrap().iter().all(|(_, opens)| !opens));
}

#[test]
fn fetches_over_ssh_with_the_password_through_askpass() {
    let askpass = Path::new("/usr/bin/fspec-install");
    let pw = Secret("pa ss".into());
    let cmd = ssh_cmd("kyan@laptop:~/.config/age/keys.txt", Some(&pw), askpass).unwrap();
    assert_eq!(cmd.args.last().map(String::as_str), Some("cat ~/'.config/age/keys.txt'"));
    assert!(cmd.args.contains(&"kyan@laptop".to_string()) && !cmd.args.contains(&"BatchMode=yes".to_string()));
    let env = |k: &str| cmd.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!((env(ASKPASS_ENV), env("SSH_ASKPASS")), (Some("pa ss"), Some("/usr/bin/fspec-install")));
    assert!(!cmd.to_string().contains("pa ss"));

    let keyless = ssh_cmd("laptop:/keys/it's.txt", None, askpass).unwrap();
    assert!(keyless.args.contains(&"BatchMode=yes".to_string()) && keyless.env.is_empty());
    assert_eq!(keyless.args.last().map(String::as_str), Some(r"cat '/keys/it'\''s.txt'"));
    for bad in ["laptop", "-oProxyCommand=x:y", "laptop:", ":/k"] {
        assert!(ssh_cmd(bad, None, askpass).is_err(), "{bad}");
    }

    let runner = RecordingRunner::default();
    runner.respond("ssh", Output { status: 0, stdout: SSH_KEY.into(), stderr: String::new() });
    let source = Source::Ssh { from: "kyan@laptop:~/.ssh/id_ed25519".into(), password: None };
    assert_eq!(fetch(&runner, &source, askpass).unwrap().kind, Kind::Ed25519);
    let denied = RecordingRunner::default();
    denied.respond("ssh", Output { status: 255, stdout: String::new(), stderr: "Permission denied".into() });
    assert!(format!("{:#}", fetch(&denied, &source, askpass).unwrap_err()).contains("Permission denied"));
}

#[test]
fn installs_age_keys_for_root_and_the_user_and_ssh_keys_for_the_user() {
    let root = "/r/run/fspec/disk/arch";
    let (a, b) = (Key::generate(), Key::generate());
    let ssh = Key::parse(SSH_KEY.as_bytes()).unwrap();
    let keys = [a.clone(), ssh, b.clone()];

    let system = system_steps(root, &keys);
    assert_eq!(system.len(), 1);
    assert_eq!(system[0].args.last().unwrap(), "/r/run/fspec/disk/arch/root/.config/fspec/age.key");
    assert_eq!(system[0].stdin.as_deref(), Some(format!("{}{}", a.private.0, b.private.0).as_str()));

    let user = user_steps(root, "kyan", &keys);
    let files: Vec<(&str, &str, &str)> =
        user.iter().map(|c| (c.args[6].as_str(), c.args[7].as_str(), c.stdin.as_deref().unwrap())).collect();
    assert_eq!(files[0].0, ".config/fspec/age.key");
    assert_eq!((files[1].0, files[1].1, files[1].2), (".ssh/id_ed25519", "600", SSH_KEY));
    assert_eq!((files[2].0, files[2].1), (".ssh/id_ed25519.pub", "644"));
    assert!(user.iter().all(|c| c.args[..2] == [root, "sh"] && c.args[5] == "kyan" && is_user_step(c)));
    assert!(user.iter().all(|c| !c.to_string().contains("AGE-SECRET-KEY") && !c.to_string().contains("PRIVATE KEY-")));

    assert!(system_steps(root, &keys[1..2]).is_empty());
    assert!(user_steps(root, "kyan", &[]).is_empty());
}

#[test]
fn says_how_to_let_the_new_key_decrypt() {
    let key = Key::generate();
    let steps = rekey_steps("nova-box", &key).join("\n");
    assert!(steps.contains(&format!("nova_box = \"{}\"", key.public[0])), "{steps}");
    assert!(steps.contains("fspec secret rekey"));
    assert_eq!(skipped_secret("warning: skipped secret /run/a: no age identity found"), Some("/run/a"));
    assert_eq!(skipped_secret("$ ls"), None);
}
