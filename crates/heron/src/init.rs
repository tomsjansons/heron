//! `heron init`: asks a few questions on the terminal and writes the config.

use std::io::{BufRead, Write};

use anyhow::{Context, Result};
use heron_core::{Config, paths};

struct Account {
    id: String,
    address: String,
    mail: bool,
    calendar: bool,
}

pub fn run(force: bool) -> Result<()> {
    let path = paths::config_file();
    anyhow::ensure!(
        force || !path.exists(),
        "{} already exists; edit it, or run `heron init --force` to start over",
        path.display()
    );

    println!("Creating {}", path.display());
    println!("Press Enter to skip a question or accept the [default].\n");

    let mut used = Vec::new();

    println!("Proton");
    let proton = match ask("  Proton address")? {
        Some(address) => Some(account(&address, "proton", &mut used)?),
        None => None,
    };

    println!("\nGoogle (one or more accounts)");
    let mut google = Vec::new();
    loop {
        let prompt = if google.is_empty() { "  Google address" } else { "  Another Google address" };
        let Some(address) = ask(prompt)? else { break };
        let suggested = suggest_id(&address);
        google.push(account(&address, &suggested, &mut used)?);
    }
    let client = if google.is_empty() {
        None
    } else {
        println!("  Google sign-in needs your own OAuth client (Desktop app) from Google Cloud;");
        println!("  the setup steps are at the top of crates/heron-google/src/lib.rs.");
        match ask("  OAuth client id")? {
            Some(id) => Some((id, ask("  OAuth client secret")?.unwrap_or_default())),
            None => None,
        }
    };

    println!("\nDiscord");
    println!("  Server id: enable Developer Mode in Discord, right-click the server, Copy Server ID.");
    let guild = ask("  Server id")?;

    let text = render(proton.as_ref(), &google, client.as_ref(), guild.as_deref());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, &text).with_context(|| format!("writing {}", path.display()))?;
    Config::load().context("the written config does not load; please report this")?;

    println!("\nWrote {}", path.display());
    println!("\nNext, log in to each source:");
    if let Some(account) = &proton {
        println!("  heron auth proton {}", account.id);
    }
    if !google.is_empty() && client.is_none() {
        println!("  (fill in [google_client] in the config first)");
    }
    for account in &google {
        println!("  heron auth google {}", account.id);
    }
    if guild.is_some() {
        println!("  heron auth discord");
    }
    println!("\nThen start the daemon:");
    println!("  cp ~/dev/heron/contrib/herond.service ~/.config/systemd/user/");
    println!("  systemctl --user enable --now herond");
    Ok(())
}

fn account(address: &str, suggested: &str, used: &mut Vec<String>) -> Result<Account> {
    let mut default = suggested.to_owned();
    let mut n = 2;
    while used.contains(&default) {
        default = format!("{suggested}{n}");
        n += 1;
    }
    let id = loop {
        let id = ask(&format!("  Account id [{default}]"))?.unwrap_or_else(|| default.clone());
        if !valid_id(&id) {
            println!("    use letters, digits, '-' or '_'");
        } else if used.contains(&id) || id == "discord" {
            println!("    {id:?} is taken");
        } else {
            break id;
        }
    };
    used.push(id.clone());
    Ok(Account {
        id,
        address: address.to_owned(),
        mail: confirm("  Show mail?")?,
        calendar: confirm("  Show calendar?")?,
    })
}

/// `me.name@gmail.com` → `me-name`
fn suggest_id(address: &str) -> String {
    let local = address.split('@').next().unwrap_or(address);
    let id: String = local
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    if valid_id(&id) { id } else { "google".into() }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn render(proton: Option<&Account>, google: &[Account], client: Option<&(String, String)>, guild: Option<&str>) -> String {
    let q = |s: &str| toml::Value::String(s.to_owned()).to_string();
    let flags = |a: &Account| {
        let mut out = String::new();
        if !a.mail {
            out.push_str("mail = false\n");
        }
        if !a.calendar {
            out.push_str("calendar = false\n");
        }
        out
    };

    let mut out = String::from("# heron config. Reference: ~/dev/heron/contrib/config.example.toml\n");
    if let Some(a) = proton {
        out += &format!("\n[[proton]]\nid = {}\nusername = {}\n{}", q(&a.id), q(&a.address), flags(a));
    }
    if !google.is_empty() {
        match client {
            Some((id, secret)) => {
                out += &format!("\n[google_client]\nclient_id = {}\nclient_secret = {}\n", q(id), q(secret));
            }
            None => out += "\n# Desktop OAuth client from Google Cloud, needed by `heron auth google`.\n\
                            # [google_client]\n# client_id = \"....apps.googleusercontent.com\"\n# client_secret = \"...\"\n",
        }
    }
    for a in google {
        out += &format!("\n[[google]]\nid = {}\nemail = {}\n{}", q(&a.id), q(&a.address), flags(a));
    }
    if let Some(guild) = guild {
        out += &format!("\n[discord]\nguild_id = {}\n", q(guild));
    }
    out
}

/// `None` for an empty answer.
fn ask(prompt: &str) -> Result<Option<String>> {
    print!("{prompt}: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line)?;
    anyhow::ensure!(read > 0, "cancelled");
    let answer = line.trim();
    Ok((!answer.is_empty()).then(|| answer.to_owned()))
}

fn confirm(prompt: &str) -> Result<bool> {
    loop {
        match ask(&format!("{prompt} [Y/n]"))?.map(|a| a.to_ascii_lowercase()) {
            None => return Ok(true),
            Some(a) if a == "y" || a == "yes" => return Ok(true),
            Some(a) if a == "n" || a == "no" => return Ok(false),
            Some(_) => println!("    answer y or n"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_config_parses() {
        let a = |id: &str, mail| Account { id: id.into(), address: format!("{id}@x.com"), mail, calendar: true };
        let text = render(
            Some(&a("proton", true)),
            &[a("work", false), a("me", true)],
            Some(&("cid".into(), "se\"cret".into())),
            Some("123"),
        );
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.google.len(), 2);
        assert!(!config.google[0].mail);
        assert_eq!(config.google_client.unwrap().client_secret, "se\"cret");
        assert_eq!(config.discord.unwrap().guild_id, "123");

        let without_client = render(None, &[a("me", true)], None, None);
        assert!(toml::from_str::<Config>(&without_client).unwrap().google_client.is_none());
    }

    #[test]
    fn ids_from_addresses() {
        assert_eq!(suggest_id("Toms.Jansons@gmail.com"), "toms-jansons");
        assert_eq!(suggest_id("a_b@x"), "a_b");
    }
}
