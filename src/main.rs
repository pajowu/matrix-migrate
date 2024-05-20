use std::time::Duration;

use clap::Parser;
use futures::{
    future::{join_all, try_join_all},
    pin_mut, try_join, StreamExt,
};
use log::{info, warn};
use matrix_sdk::{
    config::SyncSettings,
    ruma::{OwnedRoomId, OwnedServerName, OwnedUserId},
    Client,
};

/// Fast migration of one matrix account to another
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Simulate a migration. Logs in and syncs, but does not perform any actual actions
    #[arg(long = "dry-run")]
    dryrun: bool,

    /// Username of the account to migrate from
    #[arg(long = "from", env = "FROM_USER")]
    from_user: OwnedUserId,

    /// Password of the account to migrate from
    #[arg(long = "from-pw", env = "FROM_PASSWORD")]
    from_user_password: String,

    /// Custom homeserver, if not defined discovery is used
    #[arg(long, env = "FROM_HOMESERVER")]
    from_homeserver: Option<OwnedServerName>,

    /// Username of the given account to migrate to
    #[arg(long = "to", env = "TO_USER")]
    to_user: OwnedUserId,

    /// Password of the account to migrate from
    #[arg(long = "to-pw", env = "TO_PASSWORD")]
    to_user_password: String,

    /// Custom homeserver, if not defined discovery is used
    #[arg(long, env = "TO_HOMESERVER")]
    to_homeserver: Option<OwnedServerName>,

    /// Rooms to migrate (Default: all)
    #[arg(long = "rooms")]
    rooms: Vec<String>,

    /// Rooms to skip
    #[arg(long = "rooms-excluded")]
    rooms_excluded: Vec<String>,

    /// Remove old account from rooms when migration was successful
    #[arg(long = "leave-rooms")]
    leave_rooms: bool,

    /// Custom logging info
    #[arg(long, env = "RUST_LOG", default_value = "matrix_migrate=info")]
    log: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    env_logger::Builder::new().parse_filters(&args.log).init();

    if args.dryrun {
        println!("Running in dry mode, not doing any actual changes");
    }

    if !args.rooms_excluded.is_empty() {
        println!("Excluded rooms {}", args.rooms_excluded.join(", "));
    }
    if !args.rooms.is_empty() {
        println!("Only doing actions for rooms {}", args.rooms.join(", "));
    }

    let from_cb = Client::builder().user_agent("matrix-migrate/1");
    let from_c = if let Some(h) = args.from_homeserver {
        from_cb.server_name(&h).build().await?
    } else {
        from_cb
            .server_name(args.from_user.server_name())
            .build()
            .await?
    };

    info!("Logging in {:}", args.from_user);

    from_c
        .login_username(args.from_user, &args.from_user_password)
        .send()
        .await?;

    let to_cb = Client::builder().user_agent("matrix-migrate/1");
    let to_c = if let Some(h) = args.to_homeserver {
        to_cb.server_name(&h).build().await?
    } else {
        to_cb
            .server_name(args.to_user.server_name())
            .build()
            .await?
    };

    info!("Logging in {:}", args.to_user);

    to_c.login_username(args.to_user, &args.to_user_password)
        .send()
        .await?;

    info!("All logged in. Syncing...");

    let to_c_stream = to_c.clone();
    let to_sync_stream = to_c_stream.sync_stream(SyncSettings::default()).await;
    pin_mut!(to_sync_stream);

    try_join!(from_c.sync_once(SyncSettings::default()), async {
        to_sync_stream.next().await.unwrap()
    })?;

    info!("--- Synced");

    let all_prev_rooms = from_c
        .joined_rooms()
        .into_iter()
        .filter_map(|r|
            if args.rooms_excluded.contains(&r.room_id().to_string()) {
                None
            } else if !args.rooms.is_empty() && !args.rooms.contains(&r.room_id().to_string()) {
                None
            } else {
                Some(r.room_id().to_owned())
            })
        .collect::<Vec<_>>();

    let all_new_rooms = to_c
        .joined_rooms()
        .into_iter()
        .map(|r| r.room_id().to_owned())
        .chain(
            to_c.invited_rooms()
                .into_iter()
                .map(|r| r.room_id().to_owned()),
        )
        .collect::<Vec<_>>();

    let (already_invited, to_invite): (Vec<_>, Vec<_>) = all_prev_rooms
        .iter()
        .partition(|r| all_new_rooms.contains(r));

    let invites_to_accept = to_c
        .invited_rooms()
        .into_iter()
        .filter_map(|r| {
            let room_id = r.room_id().to_owned();
            if all_prev_rooms.contains(&room_id) {
                Some(room_id)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    info!(
        "--- Already sharing {}; Rooms to accept: {};  Rooms to invite: {}",
        already_invited.len(),
        invites_to_accept.len(),
        to_invite.len()
    );

    let to_user = to_c.user_id().unwrap().to_owned();
    let to_user_c = to_user.clone();
    let to_accept = invites_to_accept.iter().collect();
    let c_accept = to_c.clone();
    let ensure_user = to_user.clone();
    let ensure_c = from_c.clone();
    let inviter_c = from_c.clone();

    let (_, not_yet_accepted, (remaining_invites, failed_invites)) = try_join!(
        async move { ensure_power_levels(&ensure_c, ensure_user, &already_invited, args.dryrun).await },
        async move { accept_invites(&c_accept, &to_accept, args.dryrun).await },
        async move {
            let to_invite = to_invite.clone();
            let failed_invites = send_invites(&inviter_c, &to_invite, to_user.clone(), args.dryrun).await?;
            ensure_power_levels(&inviter_c, to_user.clone(), &to_invite, args.dryrun).await?;
            Ok((
                to_invite
                    .into_iter()
                    .map(ToOwned::to_owned)
                    .filter(|r| !failed_invites.contains(r))
                    .collect::<Vec<_>>(),
                failed_invites,
            ))
        },
    )?;

    let mut invites_awaiting = not_yet_accepted
        .into_iter()
        .chain(remaining_invites.into_iter())
        .collect::<Vec<_>>();

    info!("First invitation set done.");
    while !invites_awaiting.is_empty() && !args.dryrun {
        info!("Still {} rooms to go. Syncing up", invites_awaiting.len());
        to_sync_stream.next().await.expect("Sync stream broke")?;
        invites_awaiting = accept_invites(&to_c, &invites_awaiting.iter().collect(), args.dryrun).await?;
    }

    if !failed_invites.is_empty() {
        warn!(
            "Failed to invite to {:?}. See logs above for the reasons why",
            failed_invites
        );
    }

    if args.leave_rooms {
        let all_new_rooms = to_c
            .joined_rooms()
            .into_iter()
            .map(|r| r.room_id().to_owned())
            .collect::<Vec<_>>();

        let to_remove = all_prev_rooms
            .iter()
            .filter(|r| all_new_rooms.contains(r))
            .collect::<Vec<_>>();

        leave_room(&from_c, to_user_c.clone(), to_remove, args.dryrun).await?;
    } else {
        info!("Hint: Run again with the --leave-rooms flag to remove the old account from successfully migrated rooms");
    }

    to_c.logout().await?;
    from_c.logout().await?;

    info!("-- All done! -- ");

    Ok(())
}

async fn ensure_power_levels(
    from_c: &Client,
    new_username: OwnedUserId,
    rooms: &Vec<&OwnedRoomId>,
    dryrun: bool,
) -> anyhow::Result<()> {
    try_join_all(rooms.iter().enumerate().map(|(counter, room_id)| {
        let from_c = from_c.clone();
        let self_id = from_c.user_id().unwrap().to_owned();
        let user_id = new_username.clone();
        async move {
            if !dryrun {
                tokio::time::sleep(Duration::from_secs(counter.saturating_div(2) as u64)).await;
            }
            let Some(joined) = from_c.get_joined_room(&room_id) else {
                return anyhow::Ok(());
            };

            let Some(me) = joined.get_member(&self_id).await? else {
                warn!("{self_id} isn't member of {room_id}. Skipping power_level ensuring.");
                return anyhow::Ok(())
            };

            let Some(new_acc) = joined.get_member(&user_id).await? else {
                warn!("{user_id} isn't member of {room_id}. Skipping power_level ensuring.");
                return anyhow::Ok(())
            };

            let my_power_level = me.power_level();

            if my_power_level <= new_acc.power_level() {
                info!("Power levels of {user_id} and {self_id} in {room_id} are fine.");
                return anyhow::Ok(());
            }

            info!("Trying to adjust power_level of {user_id} in {room_id} to {my_power_level}.");

            if dryrun {
                return anyhow::Ok(());
            }

            if let Err(e) = joined
                .update_power_levels(vec![(&user_id.clone(), my_power_level.try_into().unwrap())])
                .await
            {
                warn!("Couldn't update power levels for {user_id} in {room_id}: {e}");
            }

            Ok(())
        }
    }))
    .await?;
    Ok(())
}

async fn accept_invites(
    to_c: &Client,
    rooms: &Vec<&OwnedRoomId>,
    dryrun: bool,
) -> anyhow::Result<Vec<OwnedRoomId>> {
    let mut pending = Vec::new();
    for room_id in rooms {
        let Some(invited) = to_c.get_invited_room(&room_id) else {
            if to_c.get_joined_room(room_id).is_some() { // already existing, skipping
                continue
            }
            pending.push(room_id.clone().to_owned());
            continue
        };
        info!(
            "Accepting invite for {}({})",
            invited.display_name().await?,
            invited.room_id()
        );
        if dryrun {
            continue;
        }
        invited.accept_invitation().await?;
    }

    Ok(pending)
}

async fn send_invites(
    from_c: &Client,
    rooms: &Vec<&OwnedRoomId>,
    user_id: OwnedUserId,
    dryrun: bool,
) -> anyhow::Result<Vec<OwnedRoomId>> {
    Ok(join_all(rooms.iter().enumerate().map(|(counter, room_id)| {
        let from_c = from_c.clone();
        let user_id = user_id.clone();
        async move {
            if !dryrun {
                tokio::time::sleep(Duration::from_secs(counter.saturating_div(2) as u64)).await;
            }
            let Some(joined) = from_c.get_joined_room(&room_id) else {
                        warn!("Can't invite user to {:}: not a member myself", room_id);
                        return Some(room_id.clone().to_owned());
                    };
            info!(
                "Inviting to {room_id} ({})",
                joined.display_name().await.unwrap()
            );

            if !dryrun {
                if let Err(e) = joined.invite_user_by_id(&user_id).await {
                    warn!("Inviting to {:} failed: {e}", room_id);
                    return Some(room_id.clone().to_owned());
                }
            }
            None
        }
    }))
    .await
    .into_iter()
    .filter_map(|e| e)
    .collect())
}

async fn leave_room(
    from_c: &Client,
    new_user_id: OwnedUserId,
    rooms: Vec<&OwnedRoomId>,
    dryrun: bool,
) -> anyhow::Result<()> {
    let new_user = new_user_id.clone();
    for room_id in rooms {
        // fetch room
        let Some(joined) = from_c.get_joined_room(&room_id) else {
            warn!("old user isn't member of {room_id} anymore. Skipping leave.");
            continue
        };

        // check if old user is in room
        let self_id = from_c.user_id().unwrap().to_owned();
        let Some(me) = joined.get_member(&self_id).await? else {
            warn!("old user isn't member of {room_id} anymore. Skipping leave.");
            continue
        };

        // check if new user is in room
        let Some(new_acc) = joined.get_member(&new_user).await? else {
            warn!("old user isn't member of {room_id}. Skipping leave.");
            continue
        };

        // check if new users power level is equal/greater of old user
        if me.power_level() > new_acc.power_level() {
            warn!("New user {new_user} doesn't have an equal/higher power level than {self_id} in {room_id}. Skipping leave.");
            continue
        }

        info!(
            "Leaving room {}({})",
            joined.display_name().await?,
            joined.room_id()
        );
        if dryrun {
            continue;
        }
        joined.leave().await?;
    }

    Ok(())
}
