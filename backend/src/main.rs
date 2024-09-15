#[macro_use]
extern crate rocket;

use chrono::serde::ts_seconds_option;
use chrono::{DateTime, TimeZone, Utc};
use std::env;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant};

use crate::app::App;
use crate::errors::ApiError;
use dotenvy::dotenv;
use rocket::form::Form;
use rocket::http::{CookieJar, Status};
use rocket::request::{FromRequest, Outcome};
use rocket::response::Redirect;
use rocket::serde::json::Json;
use rocket::{Request, State};
use rocket_governor::{rocket_governor_catcher, Method, Quota, RocketGovernable, RocketGovernor};
use rocket_oauth2::{OAuth2, TokenResponse};
use serde::{Deserialize, Serialize};
use sqlx::{query, query_scalar};
use uuid::Uuid;

mod minecraft;
mod app;
mod errors;
mod session_manager;

struct Discord;

#[derive(FromForm)]
struct Whitelist {
    username: String,
}

#[derive(Deserialize)]
struct DiscordCallback {
    id: String,
    username: String,
}

#[derive(Deserialize)]
struct DiscordAccessTokenResponse {
    access_token: String,
    _token_type: String,
    expires_in: i64,
    refresh_token: String,
    _scope: String,
}

#[derive(Deserialize)]
struct MinecraftUsernameToUuid {
    #[allow(dead_code)]
    name: String,
    id: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct MinecraftUuidToUsernameProperties {
    name: String,
    value: String,
}

#[derive(Deserialize)]
struct MinecraftUuidToUsername {
    #[allow(dead_code)]
    id: Uuid,
    name: String,
    properties: Vec<MinecraftUuidToUsernameProperties>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct UserData {
    pub discord_username: String,
    pub minecraft_username: String,
    pub properties: Vec<MinecraftUuidToUsernameProperties>,
}

#[derive(Serialize, Hash)]
pub struct User {
    pub discord_id: i64,
    pub minecraft_uuid: Option<String>,
    #[serde(with = "ts_seconds_option")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(with = "ts_seconds_option")]
    pub last_updated: Option<DateTime<Utc>>,
    pub is_admin: bool,
}

#[derive(Hash)]
pub struct Session {
    pub user: User,
    pub session_id: Uuid,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    pub expired: bool,
}

pub struct RateLimitGuard;
impl<'r> RocketGovernable<'r> for RateLimitGuard {
    fn quota(_method: Method, _route_name: &str) -> Quota {
        Quota::per_minute(Self::nonzero(3u32))
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Session {
    type Error = String;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let app = request.rocket().state::<App>().unwrap();

        let session_cookie = request
            .cookies()
            .get_private("session_id");

        let cookie = match session_cookie {
            Some(token) => token,
            None => {
                return Outcome::Error((
                    Status::BadRequest,
                    String::from("Session Id cookie is missing"),
                ))
            }
        };

        if let Ok(session_id) = Uuid::parse_str(cookie.value()) {
            let session = query!("SELECT * FROM sessions WHERE session_id = $1", session_id)
                .fetch_optional(&app.db)
                .await
                .unwrap();

            if let Some(session) = session {
                let user = query!("SELECT * FROM users WHERE discord_id = $1", session.user_id)
                    .fetch_optional(&app.db)
                    .await
                    .unwrap();

                if let Some(user) = user {
                    return Outcome::Success(Session {
                        user: User {
                            discord_id: user.discord_id,
                            minecraft_uuid: user.minecraft_uuid,
                            created_at: Some(user.created_at),
                            last_updated: Some(user.last_updated),
                            is_admin: user.is_admin,
                        },
                        session_id: session.session_id,
                        access_token: session.access_token,
                        refresh_token: session.refresh_token,
                        expires_at: session.expires_at,
                        expired: session.expired,
                    });
                }
            }
        }

        Outcome::Error((Status::BadRequest, "A error occurred with that request".to_string()))
    }
}

#[launch]
async fn rocket() -> _ {
    dotenv().ok();

    let app: App = App::new().await;

    sqlx::migrate!().run(&app.db).await.expect("Failed to apply migrations :(");

    rocket::build()
        .manage(app)
        .mount("/backend/", routes![discord_login, discord_logout, discord_callback, minecraft_username_change, get_user_info, get_usernames])
        .attach(OAuth2::<Discord>::fairing("discord"))
        .register("/", catchers!(rocket_governor_catcher))
}

#[get("/login/discord")]
async fn discord_login(app: &State<App>, oauth2: OAuth2<Discord>, cookies: &CookieJar<'_>) -> Redirect {
    let session_cookie = cookies.get_private("session_id");

    if let Some(cookie) = session_cookie {
        if let Ok(session_id) = Uuid::parse_str(cookie.value()) {
            let session = query!("SELECT * FROM sessions WHERE session_id = $1 AND expired = FALSE AND expires_at > NOW()", session_id)
                .fetch_optional(&app.db)
                .await
                .unwrap();

            if let Some(session) = session {
                let req = app.https.post("https://discord.com/api/oauth2/token")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .basic_auth(
                        env::var("DISCORD_CLIENT_ID").expect("Missing client id"),
                        Some(env::var("DISCORD_CLIENT_SECRET").expect("Missing client secret")),
                    )
                    .body(format!("grant_type=refresh_token&refresh_token={}", session.refresh_token))
                    .send()
                    .await
                    .unwrap()
                    .json::<DiscordAccessTokenResponse>()
                    .await
                    .unwrap();

                query!("UPDATE sessions SET expired = true WHERE session_id = $1", session_id)
                    .execute(&app.db)
                    .await
                    .unwrap();

                let session_cookie = session_manager::generate_session(app, &req.access_token, &req.refresh_token, req.expires_in).await;
                cookies.add_private(session_cookie);

                return Redirect::to("/");
            }
        }
    };

    oauth2.get_redirect(cookies, &["identify"]).unwrap()
}

#[get("/logout/discord")]
async fn discord_logout(app: &State<App>, cookies: &CookieJar<'_>) -> Redirect {
    let session_cookie = cookies.get_private("session_id");

    if let Some(cookie) = session_cookie {
        if let Ok(session_id) = Uuid::parse_str(cookie.value()) {
            let session = query!("SELECT * FROM sessions WHERE session_id = $1 AND expired = FALSE AND expires_at > NOW()", session_id)
                .fetch_optional(&app.db)
                .await
                .unwrap()
                .unwrap();

            query!("UPDATE sessions SET expired = true WHERE session_id = $1", session_id)
                .execute(&app.db)
                .await
                .unwrap();

            session_manager::revoke_discord_token(app, session.access_token).await;
            session_manager::revoke_discord_token(app, session.refresh_token).await;

            cookies.remove_private("session_id");
        }
    }

    Redirect::to("/")
}

#[get("/auth/discord")]
async fn discord_callback(app: &State<App>, token: TokenResponse<Discord>, cookies: &CookieJar<'_>) -> Result<Redirect, ApiError> {
    let Some(secs) = token.expires_in() else {
        return Err(ApiError::OptionError);
    };

    let user = app.https.get("https://discord.com/api/users/@me")
        .header("Authorization", format!("Bearer {}", token.access_token()))
        .send()
        .await
        .unwrap()
        .json::<DiscordCallback>()
        .await
        .unwrap();

    let user_id = match user.id.parse::<i64>() {
        Ok(i) => i,
        Err(err) => return Err(ApiError::ParseStringAsIntError(err))
    };

    query!("INSERT INTO users (discord_id, discord_username)
            VALUES ($1, $2)
            ON CONFLICT (discord_id) DO NOTHING;", user_id, user.username)
        .execute(&app.db)
        .await
        .unwrap();

    let session_cookie = session_manager::generate_session_with_callback(app, user, token.access_token(), token.refresh_token().unwrap(), secs).await;
    cookies.add_private(session_cookie);

    //fixme change redirect location
    Ok(Redirect::to("/"))
}

#[post("/minecraft/username/change", data = "<whitelist_data>")]
async fn minecraft_username_change(app: &State<App>, _limit_guard: RocketGovernor<'_, RateLimitGuard>, session: Session, whitelist_data: Form<Whitelist>) -> Result<(), ApiError> {
    let release_date = Utc.with_ymd_and_hms(2024, 10, 16, 5, 0, 0).unwrap();

    let current_date_time = Utc::now();

    if current_date_time < release_date {
        return Err(ApiError::OptionError);
    }

    let request = app.https.get(format!("https://api.mojang.com/users/profiles/minecraft/{}", whitelist_data.username))
        .send()
        .await;

    return match request {
        Ok(req) => {
            let user_profile = req
                .json::<MinecraftUsernameToUuid>()
                .await;

            match user_profile {
                Ok(profile) => {
                    let query = query_scalar!("UPDATE users SET minecraft_uuid = $1 WHERE discord_id = $2 RETURNING minecraft_uuid", profile.id, session.user.discord_id)
                        .fetch_one(&app.db)
                        .await;

                    match query {
                        Ok(result) => {
                            if let Some(old_username) = result {
                                minecraft::minecraft_whitelist_remove(app, old_username).await;
                            }
                            minecraft::minecraft_whitelist(app, &whitelist_data).await;

                            Ok(())
                        }
                        Err(err) => Err(ApiError::SQL(err)),
                    }
                }
                Err(err) => Err(ApiError::Request(err))
            }
        }
        Err(err) => Err(ApiError::Request(err))
    };
}

#[get("/users/@me")]
async fn get_user_info(session: Session) -> Json<User> {
    Json(session.user)
}

#[get("/users/id_to_username/<uuid>")]
async fn get_usernames(app: &State<App>, session: Session, uuid: String) -> Json<UserData> {
    let mut hasher = DefaultHasher::new();
    session.hash(&mut hasher);
    let cache_key = hasher.finish();
    let cache_duration = Duration::new(3600, 0);

    {
        let mut cache = app.cache.write().unwrap();
        cache.retain(|_, (_, timestamp)| timestamp.elapsed() < cache_duration);
        if let Some((data, timestamp)) = cache.get(&cache_key) {
            if timestamp.elapsed() < cache_duration {
                let deserialized: UserData = serde_json::from_str(&data).unwrap();
                return Json(deserialized.clone());
            }
        }
    }

    let discord_user = app.https.get(format!("https://discord.com/api/users/{}", session.user.discord_id))
        .send()
        .await
        .unwrap()
        .json::<DiscordCallback>()
        .await
        .unwrap();

    let mc_profile = app.https.get(format!("https://sessionserver.mojang.com/session/minecraft/profile/{}", uuid))
        .send()
        .await
        .unwrap()
        .json::<MinecraftUuidToUsername>()
        .await
        .unwrap();

    let data = UserData {
        discord_username: discord_user.username,
        minecraft_username: mc_profile.name,
        properties: mc_profile.properties,
    };

    {
        let mut write_cache = app.cache.write().unwrap();
        write_cache.insert(cache_key, (serde_json::to_string(&data).unwrap(), Instant::now()));
    }

    Json(data)
}