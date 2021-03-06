use anyhow::{Context as _, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use chrono_humanize::HumanTime;
use clap::Clap;
use config::Config;
use modul::{Modul, ModulGruppe, ModulTermin};
use serenity::client::{Client, Context, EventHandler};
use serenity::framework::standard::macros::{command, group, help};
use serenity::framework::standard::{
    help_commands, Args, CommandGroup, CommandResult, HelpOptions, StandardFramework,
};
use serenity::model::{channel::Message, id::ChannelId, id::UserId};
use serenity::prelude::TypeMapKey;
use std::sync::{Arc, Mutex};
use std::{collections::HashSet, fs, io, thread, time::Duration};
use user::Users;

mod arg;
mod config;
mod modul;
mod scraper;
mod user;

const SLEEP_SECS: u64 = 60;

struct Data {
    module: Vec<Modul>,
    users: Users,
}

impl TypeMapKey for Data {
    type Value = Arc<Mutex<Data>>;
}

#[group]
#[commands(list, update)]
struct General;

#[group]
#[prefixes("dm")]
#[commands(enable, disable, remove, set, get)]
struct DirectMessages;

#[command]
fn list(ctx: &mut Context, msg: &Message, args: Args) -> CommandResult {
    let map = ctx.data.read();
    let config = map.get::<Config>().unwrap();
    let data = map.get::<Data>().unwrap();
    let data_lock = data.lock().unwrap();

    let author_group = match data_lock.users.get(msg.author.id) {
        Some(user) => user.gruppe.clone(),
        None => {
            let user_has_role = |role_id: u64| match msg.author.has_role(
                &ctx.http,
                config.discord.guild_id,
                role_id,
            ) {
                Ok(v) => v,
                Err(_) => false,
            };
            if user_has_role(config.discord.gruppe_1.role_id) {
                Some(ModulGruppe::Gruppe1)
            } else if user_has_role(config.discord.gruppe_2.role_id) {
                Some(ModulGruppe::Gruppe2)
            } else if user_has_role(config.discord.gruppe_3.role_id) {
                Some(ModulGruppe::Gruppe3)
            } else if user_has_role(config.discord.gruppe_4.role_id) {
                Some(ModulGruppe::Gruppe4)
            } else {
                None
            }
        }
    };

    let get_messages = |filter: Box<dyn Fn(&ModulTermin) -> bool>| {
        data_lock
            .module
            .iter()
            .flat_map(|modul| modul.messages(|termin| filter(termin)))
            .filter(|message| {
                message.modul.gruppe.is_none() || message.modul.gruppe == author_group
            })
            .collect::<Vec<_>>()
    };

    let mut messages = match args.current() {
        Some(arg) => match NaiveDate::parse_from_str(arg, "%d.%m.%Y") {
            Ok(v) => {
                let date = chrono_tz::Europe::Berlin.from_local_date(&v).unwrap();
                let messages = get_messages(Box::new(|termin| termin.beginn.date() == date));
                if messages.is_empty() {
                    msg.channel_id.send_message(&ctx.http, |m| {
                        m.content(format!(
                            "Keine Lehrveranstaltungen am {}",
                            date.format("%d.%m.%Y")
                        ))
                    })?;
                    return Ok(());
                }
                messages
            }
            Err(_) => {
                msg.reply(&ctx.http, "Error: Invalid date format")?;
                return Ok(());
            }
        },
        None => {
            let mut date = Utc::now().date();
            let mut messages = Vec::new();
            for _ in 0..7 {
                let date2 = date;
                messages.extend(get_messages(Box::new(move |termin| {
                    termin.beginn.date() == date2 && termin.ende > Utc::now()
                })));
                if !messages.is_empty() {
                    break;
                }
                date = date + chrono::Duration::days(1);
            }
            messages
        }
    };

    messages.sort_by_key(|m| m.modul_termin.beginn);
    for message in messages {
        msg.channel_id
            .send_message(&ctx.http, |m| message.to_create_message(m, &config))?;
    }
    Ok(())
}

/// Enables direct messages
#[command]
fn enable(ctx: &mut Context, msg: &Message) -> CommandResult {
    let mut map = ctx.data.write();
    let data = map.get_mut::<Data>().unwrap();
    match data.lock().unwrap().users.enable(msg.author.id) {
        Ok(_) => msg.reply(&ctx.http, "Enabled direct messages")?,
        Err(e) => msg.reply(&ctx.http, format!("Error: {}", e))?,
    };
    Ok(())
}

/// Disables direct messages
#[command]
fn disable(ctx: &mut Context, msg: &Message) -> CommandResult {
    let mut map = ctx.data.write();
    let data = map.get_mut::<Data>().unwrap();
    match data.lock().unwrap().users.disable(msg.author.id) {
        Ok(_) => msg.reply(&ctx.http, "Disabled direct messages")?,
        Err(e) => msg.reply(&ctx.http, format!("Error: {}", e))?,
    };
    Ok(())
}

/// Disables direct messages and removes the configuration
#[command]
fn remove(ctx: &mut Context, msg: &Message) -> CommandResult {
    let mut map = ctx.data.write();
    let data = map.get_mut::<Data>().unwrap();
    match data.lock().unwrap().users.remove(msg.author.id) {
        Ok(_) => msg.reply(
            &ctx.http,
            "Disabled direct messages and removed configuration",
        )?,
        Err(e) => msg.reply(&ctx.http, format!("Error: {}", e))?,
    };
    Ok(())
}

/// Modifies configuration options for direct messages
///
/// Available subcommands:
/// - `send-before`: Takes either a number or `off` as value
/// - `send-after-previous`: Takes either `on` or `off` as value
/// - `group`: Takes either `1`, `2`, `3`, `4`, or `none` as value
#[command]
fn set(ctx: &mut Context, msg: &Message, mut args: Args) -> CommandResult {
    let mut map = ctx.data.write();
    let data = map.get_mut::<Data>().unwrap();
    match args.single::<String>().unwrap().as_str() {
        "send-before" => {
            let arg = args.single::<String>().unwrap();
            let duration = match arg.as_str() {
                "off" => None,
                v => match v.parse::<u64>() {
                    Ok(v) => Some(user::Duration { minutes: v }),
                    Err(_) => {
                        msg.reply(
                            &ctx.http,
                            format!(
                                "Error: Unknown value `{}` (available values: number, `off`)",
                                v
                            ),
                        )?;
                        return Ok(());
                    }
                },
            };
            match data
                .lock()
                .unwrap()
                .users
                .set_send_before(msg.author.id, duration)
            {
                Ok(_) => msg.reply(&ctx.http, format!("Set `send-before` to `{}`", arg))?,
                Err(e) => msg.reply(&ctx.http, format!("Error: {}", e))?,
            };
        }
        "send-after-previous" => {
            let arg = args.single::<String>().unwrap();
            let enable = match arg.as_str() {
                "off" => false,
                "on" => true,
                v => {
                    msg.reply(
                        &ctx.http,
                        format!(
                            "Error: Unknown value `{}` (available values: `on`, `off`)",
                            v
                        ),
                    )?;
                    return Ok(());
                }
            };
            match data
                .lock()
                .unwrap()
                .users
                .set_send_after(msg.author.id, enable)
            {
                Ok(_) => msg.reply(&ctx.http, format!("Set `send-after-previous` to `{}`", arg))?,
                Err(e) => msg.reply(&ctx.http, format!("Error: {}", e))?,
            };
        }
        "group" => {
            let (arg, group) = match args.single::<String>().as_deref() {
                Ok("1") => ("1", Some(ModulGruppe::Gruppe1)),
                Ok("2") => ("2", Some(ModulGruppe::Gruppe2)),
                Ok("3") => ("3", Some(ModulGruppe::Gruppe3)),
                Ok("4") => ("4", Some(ModulGruppe::Gruppe4)),
                Ok("none") => ("none", None),
                Ok(_) | Err(_) => {
                    msg.reply(
                        &ctx.http,
                        "Error: Invalid value (available values: `1`, `2`, `3`, `4`, `none`)",
                    )?;
                    return Ok(());
                }
            };
            match data.lock().unwrap().users.set_group(msg.author.id, group) {
                Ok(_) => msg.reply(&ctx.http, format!("Set `group` to `{}`", arg))?,
                Err(e) => msg.reply(&ctx.http, format!("Error: {}", e))?,
            };
        }
        v => {
            msg.reply(&ctx.http, format!("Error: Unknown subcommand `{}`", v))?;
            return Ok(());
        }
    };
    Ok(())
}

/// Displays the configuration
#[command]
fn get(ctx: &mut Context, msg: &Message) -> CommandResult {
    let map = ctx.data.read();
    let data = map.get::<Data>().unwrap();
    match data.lock().unwrap().users.get(msg.author.id) {
        Some(user) => {
            let send_before_fmt = match &user.send_before {
                Some(v) => format!("{}min", v.minutes),
                None => "off".to_owned(),
            };
            let send_after_previous_fmt = if user.send_after_previous {
                "on"
            } else {
                "off"
            };
            let group = match &user.gruppe {
                Some(v) => v.to_string(),
                None => "none".to_owned(),
            };
            msg.channel_id.send_message(&ctx.http, |m| {
                m.embed(|e| {
                    e.title("Configuration")
                        .field("enabled", user.enabled, false)
                        .field("send-before", send_before_fmt, false)
                        .field("send-after-previous", send_after_previous_fmt, false)
                        .field("group", group, false)
                })
            })?;
        }
        None => {
            msg.reply(
                &ctx.http,
                "Error: User not found (DMs can be enabled with `@lsfbot dm enable`)",
            )?;
        }
    };
    Ok(())
}

#[command]
fn update(ctx: &mut Context, msg: &Message) -> CommandResult {
    let map = ctx.data.read();
    let config = map.get::<Config>().unwrap();
    let module = match scraper::fetch_module(&config) {
        Ok(v) => v,
        Err(e) => {
            msg.reply(&ctx.http, format!("Error: {}", e))?;
            return Ok(());
        }
    };
    drop(map);
    let mut map = ctx.data.write();
    let data = map.get_mut::<Data>().unwrap();
    data.lock().unwrap().module = module;
    msg.reply(&ctx.http, "Stundenplan wurde aktualisiert")?;
    Ok(())
}

#[help]
fn help(
    context: &mut Context,
    msg: &Message,
    args: Args,
    help_options: &'static HelpOptions,
    groups: &[&'static CommandGroup],
    owners: HashSet<UserId>,
) -> CommandResult {
    help_commands::with_embeds(context, msg, args, help_options, groups, owners)
}

struct Handler;

impl EventHandler for Handler {}

fn main() -> Result<()> {
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} [{:>5}] {}: {}",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S"),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Warn)
        .level_for("lsfbot", log::LevelFilter::Trace)
        .chain(io::stdout())
        .apply()?;

    let args = arg::Args::parse();

    let config_data = fs::read_to_string(&args.config).context("Failed to read config file")?;
    let config: Config =
        toml::from_str(&config_data).context("Failed to deserialize config file")?;
    let config = Arc::new(config);

    let users_file_path = if config.users.file.is_absolute() {
        config.users.file.clone()
    } else {
        args.config.parent().unwrap().join(&config.users.file)
    };

    let mut client = Client::new(&config.discord.bot_token, Handler).unwrap();

    let data = Arc::new(Mutex::new(Data {
        module: Vec::new(),
        users: Users::new(
            users_file_path,
            Arc::clone(&config),
            Arc::clone(&client.cache_and_http),
        )
        .context("Failed to read users file")?,
    }));

    let http_client = Arc::clone(&client.cache_and_http.http);
    let bot_id = http_client.get_current_user().unwrap().id;
    let framework = StandardFramework::new()
        .configure(|c| c.on_mention(Some(bot_id)))
        .help(&HELP)
        .group(&GENERAL_GROUP)
        .group(&DIRECTMESSAGES_GROUP);
    client.with_framework(framework);

    {
        let mut client_data = client.data.write();
        client_data.insert::<Config>(Arc::clone(&config));
        client_data.insert::<Data>(Arc::clone(&data));
    }

    let start_client_join_handle = thread::spawn(move || {
        log::debug!("Starting discord client");
        client.start()
    });

    let module = scraper::fetch_module(&config).context("Failed to fetch data from website")?;
    data.lock().unwrap().module = module;

    let reminder_join_handle = thread::spawn(move || {
        log::debug!("Checking for reminders");

        let send_message =
            |message: &modul::MessageData, group: &config::Group| match ChannelId(group.channel_id)
                .send_message(&http_client, |m| {
                    message
                        .to_create_message(m, &config)
                        .content(format!("<@&{}>", group.role_id))
                }) {
                Ok(_) => log::info!("Sent reminder message to channel `{}`", group.channel_id),
                Err(e) => log::error!("Failed to send reminder message: {}", e),
            };

        let mut delay = chrono::Duration::zero();
        loop {
            let start = Utc::now();
            log::debug!("Starting loop for reminder messages");
            let mut data_lock = data.lock().unwrap();
            data_lock.users.refresh().unwrap();
            let module = &data_lock.module;

            log::debug!("Checking messages for group channels");
            let mut messages = module
                .iter()
                .flat_map(|modul| {
                    modul.messages(|termin| {
                        let duration = termin.beginn.signed_duration_since(Utc::now());
                        duration.num_seconds() > 30 * 60 - SLEEP_SECS as i64 - delay.num_seconds()
                            && duration.num_seconds() < 30 * 60
                    })
                })
                .collect::<Vec<_>>();
            messages.sort_by_key(|m| m.modul_termin.beginn);
            for message in messages {
                match message.modul.gruppe {
                    Some(ModulGruppe::Gruppe1) => send_message(&message, &config.discord.gruppe_1),
                    Some(ModulGruppe::Gruppe2) => send_message(&message, &config.discord.gruppe_2),
                    Some(ModulGruppe::Gruppe3) => send_message(&message, &config.discord.gruppe_3),
                    Some(ModulGruppe::Gruppe4) => send_message(&message, &config.discord.gruppe_4),
                    None => {
                        send_message(&message, &config.discord.gruppe_1);
                        send_message(&message, &config.discord.gruppe_2);
                        send_message(&message, &config.discord.gruppe_3);
                        send_message(&message, &config.discord.gruppe_4);
                    }
                }
            }
            log::debug!("Finished checking messages for group channels");

            log::debug!("Checking for user messages");
            for user in data_lock.users.get_all() {
                if !user.enabled {
                    log::debug!("Skipping messages for user `{}` (disabled)", user.id);
                    continue;
                }
                log::debug!("Checking messages for user `{}`", user.id);
                let messages = module
                    .iter()
                    .flat_map(|modul| {
                        modul.messages(|termin| {
                            let duration = termin.beginn.signed_duration_since(Utc::now());
                            match user.send_before.as_ref().map(|v| v.minutes) {
                                Some(minutes) => {
                                    duration.num_seconds()
                                        > (minutes * 60 - SLEEP_SECS) as i64 - delay.num_seconds()
                                        && duration.num_seconds() < (minutes * 60) as i64
                                        && (modul.gruppe.is_none() || modul.gruppe == user.gruppe)
                                }
                                None => false,
                            }
                        })
                    })
                    .collect::<Vec<_>>();
                log::debug!("Finished checking for messages for user `{}`", user.id);

                if !messages.is_empty() {
                    log::debug!("Creating dm channel for user `{}`", user.id);
                    let channel = match user.id.create_dm_channel(&http_client) {
                        Ok(v) => v,
                        Err(e) => {
                            log::error!(
                                "Failed to create dm channel for user `{}`: {}",
                                user.id,
                                e
                            );
                            continue;
                        }
                    };
                    for message in messages {
                        match channel
                            .send_message(&http_client, |m| message.to_create_message(m, &config))
                        {
                            Ok(_) => {
                                log::info!("Sent reminder message to dm channel `{}`", channel.id.0)
                            }
                            Err(e) => log::error!("Failed to send reminder message: {}", e),
                        };
                    }
                }

                if user.send_after_previous {
                    log::debug!(
                        "Checking for `send-after-previous` message for user `{}`",
                        user.id
                    );
                    let messages_today = module
                        .iter()
                        .flat_map(|modul| {
                            modul.messages(|termin| {
                                termin.beginn.date() == Utc::now().date()
                                    && (modul.gruppe.is_none() || modul.gruppe == user.gruppe)
                            })
                        })
                        .collect::<Vec<_>>();
                    let last = messages_today
                        .iter()
                        .filter(|v| v.modul_termin.beginn < Utc::now())
                        .map(|v| v.modul_termin.ende)
                        .find(|v| {
                            let duration = v.signed_duration_since(Utc::now());
                            duration.num_seconds() > 0
                                && duration.num_seconds() < SLEEP_SECS as i64 + delay.num_seconds()
                        });
                    let next_message = last.and_then(|last| {
                        messages_today
                            .into_iter()
                            .filter(|v| v.modul_termin.beginn > last)
                            .min_by_key(|v| v.modul_termin.beginn)
                    });
                    if let Some(message) = next_message {
                        log::debug!("Creating dm channel for user `{}`", user.id);
                        let channel = match user.id.create_dm_channel(&http_client) {
                            Ok(v) => v,
                            Err(e) => {
                                log::error!(
                                    "Failed to create dm channel for user `{}`: {}",
                                    user.id,
                                    e
                                );
                                continue;
                            }
                        };
                        match channel.send_message(&http_client, |m| {
                            let duration = HumanTime::from(
                                message
                                    .modul_termin
                                    .beginn
                                    .signed_duration_since(Utc::now()),
                            );
                            message
                                .to_create_message(m, &config)
                                .content(duration.to_text_en(
                                    chrono_humanize::Accuracy::Precise,
                                    chrono_humanize::Tense::Future,
                                ))
                        }) {
                            Ok(_) => {
                                log::info!("Sent reminder message to dm channel `{}`", channel.id.0)
                            }
                            Err(e) => log::error!("Failed to send reminder message: {}", e),
                        };
                    }
                }
                log::debug!("Finished checks for user `{}`", user.id);
            }
            log::debug!("Finished checks");
            drop(data_lock);
            delay = Utc::now().signed_duration_since(start);
            thread::sleep(Duration::from_secs(SLEEP_SECS));
        }
    });

    start_client_join_handle.join().unwrap()?;
    reminder_join_handle.join().unwrap();

    Ok(())
}
