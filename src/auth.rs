//! Device-bound QQ music accounts and credential-scoped requests.
mod login;

#[cfg(test)]
pub(crate) mod tests;

use crate::{device, model, Error, Host, Response, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

const MUSICU: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const HOUR: u64 = 3_600_000;

#[derive(Default)]
pub struct Auth {
    restored: Option<Option<Value>>,
    login: Option<login::Login>,
    http: HttpClient,
    cdn: Option<(String, u64)>,
    failures: u32,
    next_login_at: u64,
    login_failure_recorded: bool,
}

impl Auth {
    pub fn session(&mut self, host: &impl Host) -> Result<Option<Value>> {
        if let Some(session) = self.login.as_ref().and_then(|value| value.issued.as_ref()) {
            return Ok(Some(session.clone()));
        }
        if let Some(restored) = &self.restored {
            return Ok(restored.clone());
        }
        let bytes = host.credential_read()?;
        if bytes.is_empty() {
            self.restored = Some(None);
            return Ok(None);
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| Error::new("invalidData", "account.credential-json"))?;
        if !valid_session(&value) {
            return Err(Error::new("invalidData", "account.credential-shape"));
        }
        self.restored = Some(Some(value.clone()));
        Ok(Some(value))
    }

    pub fn prepare_login(&mut self, host: &impl Host, request: &Value) -> Result<()> {
        let method = request["method"].as_str().unwrap_or("qq");
        if !matches!(method, "qq" | "wechat") {
            return Err(Error::new("unsupported", "login.method"));
        }
        if let Some(login) = &self.login {
            if login.method != method {
                return Err(Error::new("staleContext", "login.method-context"));
            }
            return Ok(());
        }
        if host.now_ms() < self.next_login_at {
            return Err(Error::new("rateLimited", "login.backoff")
                .retry_after(self.next_login_at.saturating_sub(host.now_ms())));
        }
        let result: Result<login::Login> = (|| {
            let device = device::prepare(host)?;
            login::Login::new(host, method, device)
        })();
        match result {
            Ok(login) => {
                self.login = Some(login);
                self.login_failure_recorded = false;
                Ok(())
            }
            Err(error) => {
                self.backoff(host);
                Err(error.retry_after(self.next_login_at.saturating_sub(host.now_ms())))
            }
        }
    }

    fn backoff(&mut self, host: &impl Host) {
        self.failures = self.failures.saturating_add(1);
        let delay = 30_000_u64
            .saturating_mul(1_u64 << self.failures.saturating_sub(1).min(5))
            .min(900_000);
        self.next_login_at = host.now_ms().saturating_add(delay);
    }

    pub fn account(&mut self, host: &impl Host, request: &Value) -> Result<Value> {
        match request["operation"].as_str() {
            Some("beginLogin") => {
                self.prepare_login(host, request)?;
                let login = self
                    .login
                    .as_mut()
                    .ok_or_else(|| Error::new("invalidData", "login.missing"))?;
                match login.create_qr(host) {
                    Ok(()) => {
                        Ok(json!({"type":"challenge","data":{"key":login.key,"image":login.image}}))
                    }
                    Err(error) => {
                        self.backoff(host);
                        self.login_failure_recorded = true;
                        Err(error.retry_after(self.next_login_at.saturating_sub(host.now_ms())))
                    }
                }
            }
            Some("pollLogin") => {
                let mut login = self
                    .login
                    .take()
                    .ok_or_else(|| Error::new("staleContext", "login.challenge"))?;
                if request["key"] != login.key {
                    self.login = Some(login);
                    return Err(Error::new("staleContext", "login.challenge"));
                }
                let advance = login.advance(host, 1000);
                self.login = Some(login);
                advance?;
                let login = self.login.as_mut().unwrap();
                let code = login.progress();
                if code == 803 && !login.published {
                    let session = login
                        .issued
                        .as_ref()
                        .ok_or_else(|| Error::new("invalidData", "login.issue"))?
                        .clone();
                    let profile = profile_raw(host, &mut self.http, &session)?;
                    let account = model::account_record(&profile, &session["credential"])?;
                    host.credential_publish(&account.to_string(), session.to_string().as_bytes())?;
                    login.published = true;
                    self.failures = 0;
                    self.next_login_at = 0;
                }
                if code == 800 && !self.login_failure_recorded {
                    self.backoff(host);
                    self.login_failure_recorded = true;
                }
                Ok(
                    json!({"type":"progress","data":{"code":code,"message":match code {
                        803 => "登录成功", 802 => "已扫码，请在手机确认", 800 => "二维码失效或登录失败", _ => "等待扫码"
                    }}}),
                )
            }
            Some("profile") => {
                let session = self.session(host)?;
                if let Some(session) = session {
                    let profile = self.profile(host)?;
                    Ok(
                        json!({"type":"profile","data":model::account_record(&profile, &session["credential"])?}),
                    )
                } else {
                    Ok(json!({"type":"profile","data":null}))
                }
            }
            _ => Err(Error::new("unsupported", "account.operation")),
        }
    }

    pub fn event(&mut self, host: &impl Host, value: &Value) -> Result<()> {
        if let Some(login) = &mut self.login {
            if value["kind"] == "tick" && login.method != "qq" {
                return Ok(());
            }
            login.event(host, value)?;
            if login.progress() == 800 && !self.login_failure_recorded {
                self.backoff(host);
                self.login_failure_recorded = true;
            }
        }
        Ok(())
    }

    pub fn shutdown(&mut self, host: &impl Host) {
        if let Some(login) = &mut self.login {
            login.close(host);
        }
        self.login = None;
        self.restored = None;
        self.http.clear();
        self.cdn = None;
        self.login_failure_recorded = false;
    }

    fn with_session<T>(
        &mut self,
        host: &impl Host,
        operation: impl FnOnce(&mut HttpClient, &Value) -> Result<T>,
    ) -> Result<T> {
        let session = self
            .session(host)?
            .ok_or_else(|| Error::new("unauthenticated", "account.session"))?;
        if session["expiresAt"].as_f64().unwrap_or(0.0) > host.now_ms() as f64 {
            match operation(&mut self.http, &session) {
                Ok(value) => return Ok(value),
                Err(error) if error.code == "unauthenticated" => {}
                Err(error) => return Err(error),
            }
        }
        self.refresh(host, &session)?;
        Err(Error::new("staleContext", "account.refreshed"))
    }

    fn refresh(&mut self, host: &impl Host, session: &Value) -> Result<()> {
        let credential = &session["credential"];
        let result = raw_rpc(
            host,
            &mut self.http,
            &session["device"],
            Some(credential),
            "music.login.LoginServer",
            "Login",
            json!({"openid":string(&credential["openid"]),"access_token":string(&credential["access_token"]),
                "refresh_token":string(&credential["refresh_token"]),"expired_in":credential["expired_at"],
                "str_musicid":music_id(credential),"musicid":credential["musicid"],"musickey":credential["musickey"],
                "unionid":string(&credential["unionid"]),"refresh_key":string(&credential["refresh_key"]),"loginMode":2}),
            json!({"tmeLoginType":credential["loginType"]}),
            0,
        );
        let next = match result {
            Ok(value) => credential_from(value, credential["loginType"].as_i64().unwrap_or(6))?,
            Err(error) if error.code == "unauthenticated" => {
                host.credential_invalidate()?;
                self.restored = Some(None);
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        if music_id(&next) != music_id(credential) {
            return Err(Error::new("invalidData", "account.refresh-identity"));
        }
        let next_session = json!({"token":"current","device":session["device"],
            "expiresAt":expiry(&next,host.now_ms()),"credential":next});
        if !valid_session(&next_session) {
            return Err(Error::new("invalidData", "account.refresh-shape"));
        }
        host.credential_replace(next_session.to_string().as_bytes())?;
        self.restored = None;
        self.cdn = None;
        Ok(())
    }

    pub fn rpc(
        &mut self,
        host: &impl Host,
        module: &str,
        method: &str,
        param: Value,
    ) -> Result<Value> {
        self.with_session(host, |http, session| {
            raw_rpc(
                host,
                http,
                &session["device"],
                Some(&session["credential"]),
                module,
                method,
                param,
                json!({}),
                0,
            )
        })
    }

    pub fn profile(&mut self, host: &impl Host) -> Result<Value> {
        self.with_session(host, |http, session| profile_raw(host, http, session))
    }

    pub fn liked_songs(&mut self, host: &impl Host, offset: usize, limit: usize) -> Result<Value> {
        self.owned_playlist_songs(
            host,
            json!({"disstid":0,"dirid":201,"offset":offset,"limit":limit}),
        )
    }

    pub fn owned_playlist_songs(&mut self, host: &impl Host, request: Value) -> Result<Value> {
        let session = self
            .session(host)?
            .ok_or_else(|| Error::new("unauthenticated", "playlist.session"))?;
        let encrypted = string(&session["credential"]["encryptUin"]);
        if encrypted.is_empty() {
            return Err(Error::new("invalidData", "playlist.encrypted-uin"));
        }
        let offset = request["offset"].as_u64().unwrap_or(0);
        let limit = request["limit"].as_u64().unwrap_or(100).clamp(1, 100);
        let result = self.rpc(host, "music.srfDissInfo.DissInfo", "CgiGetDiss", json!({
            "disstid":request["disstid"].as_u64().unwrap_or(0),"dirid":request["dirid"].as_u64().unwrap_or(0),
            "tag":true,"song_begin":offset,"song_num":limit,"userinfo":true,"orderlist":true,"enc_host_uin":encrypted
        }))?;
        Ok(public_value(result))
    }

    pub fn user_albums(&mut self, host: &impl Host, offset: usize, limit: usize) -> Result<Value> {
        self.with_session(host, |http, session| {
            let limit = limit.clamp(1, 100);
            let mut url =
                Url::parse("https://c.y.qq.com/fav/fcgi-bin/fcg_get_profile_order_asset.fcg")
                    .map_err(|_| Error::new("invalidData", "albums.url"))?;
            url.query_pairs_mut().extend_pairs([
                ("ct", "20".into()),
                ("cid", "205360956".into()),
                ("userid", music_id(&session["credential"])),
                ("reqtype", "2".into()),
                ("sin", offset.to_string()),
                ("ein", (offset + limit - 1).to_string()),
                ("format", "json".into()),
            ]);
            let response = http.request(
                host,
                "GET",
                url.as_str(),
                vec![
                    ("Referer".into(), "https://y.qq.com/".into()),
                    ("Cookie".into(), credential_cookie(&session["credential"])),
                ],
                Vec::new(),
                0,
            )?;
            let body = response_json(&response, "albums.json")?;
            let code = model::safe_integer(&body["code"])
                .ok_or_else(|| Error::new("invalidData", "albums.code"))?;
            if code != 0 {
                return Err(upstream_error(code, "albums"));
            }
            Ok(public_value(body["data"].clone()))
        })
    }

    pub fn playback(
        &mut self,
        host: &impl Host,
        mid: &str,
        quality: &str,
        media_mid: &str,
    ) -> Result<Value> {
        let (prefix, extension) = match quality {
            "320" | "exhigh" => ("M800", ".mp3"),
            "flac" | "lossless" => ("F000", ".flac"),
            "ape" => ("A000", ".ape"),
            "m4a" => ("C400", ".m4a"),
            _ => ("M500", ".mp3"),
        };
        let guid = crate::crypto::hex(&host.random(16)?);
        let session = self
            .session(host)?
            .ok_or_else(|| Error::new("unauthenticated", "playback.session"))?;
        let data = self.rpc(host,"music.vkey.GetVkey","UrlGetVkey",json!({
            "filename":[format!("{prefix}{mid}{}{extension}",if media_mid.is_empty(){mid}else{media_mid})],
            "guid":guid,"songmid":[mid],"songtype":[0],"uin":music_id(&session["credential"]),"ctx":0
        }))?;
        let entries = data["midurlinfo"].as_array().cloned().unwrap_or_default();
        if entries.is_empty() {
            host.diagnostic(
                &format!("playback.vkey-entries.{prefix}"),
                "EntriesMissing",
                &safe_upstream(&data["code"]),
            );
        }
        for entry in &entries {
            if string(&entry["songmid"]) != mid {
                host.diagnostic(
                    &format!("playback.vkey-identity.{prefix}"),
                    "UnmatchedSong",
                    &safe_upstream(&entry["result"]),
                );
            } else if string(&entry["purl"]).is_empty() {
                host.diagnostic(
                    &format!("playback.vkey-empty.{prefix}"),
                    "EmptyPurl",
                    &safe_upstream(&entry["result"]),
                );
            }
        }
        let sip = data["sip"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(string)
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>();
        let mut domain = sip
            .iter()
            .find(|v| !v.starts_with("http://ws"))
            .or(sip.first())
            .cloned();
        if domain.is_none() {
            if let Some(purl) = entries
                .iter()
                .map(|v| string(&v["purl"]))
                .find(|v| !v.is_empty())
            {
                domain = self.select_cdn(
                    host,
                    &["http://sjy6.stream.qqmusic.qq.com/".into()],
                    &purl,
                    1_800_000,
                )?;
                if domain.is_none() {
                    let dispatch = self.rpc(
                        host,
                        "music.audioCdnDispatch.cdnDispatch",
                        "GetCdnDispatch",
                        json!({"guid":guid,"uid":"0","use_new_domain":1,"use_ipv6":1}),
                    );
                    match dispatch {
                        Ok(dispatch) => {
                            let mut candidates = dispatch["sip"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default()
                                .iter()
                                .map(string)
                                .filter(|v| {
                                    Url::parse(v)
                                        .ok()
                                        .and_then(|u| u.host_str().map(str::to_owned))
                                        .as_deref()
                                        != Some("sjy6.stream.qqmusic.qq.com")
                                })
                                .collect::<Vec<_>>();
                            candidates.push("http://dl.stream.qqmusic.qq.com/".into());
                            domain = self.select_cdn(
                                host,
                                &candidates,
                                &purl,
                                dispatch["refreshTime"]
                                    .as_u64()
                                    .unwrap_or(1800)
                                    .saturating_mul(1000),
                            )?;
                        }
                        Err(error)
                            if matches!(
                                error.code.as_str(),
                                "cancelled"
                                    | "staleContext"
                                    | "permissionDenied"
                                    | "unauthenticated"
                            ) =>
                        {
                            return Err(error)
                        }
                        Err(_) => {}
                    }
                }
            }
        }
        let domain = domain.unwrap_or_else(|| "http://dl.stream.qqmusic.qq.com/".into());
        let mut output = serde_json::Map::new();
        for entry in entries {
            let id = string(&entry["songmid"]);
            if id.is_empty() {
                continue;
            }
            let purl = string(&entry["purl"]);
            output.insert(id,json!({"url":if purl.is_empty(){String::new()}else{format!("{domain}{purl}")},"error":purl.is_empty()}));
        }
        Ok(Value::Object(output))
    }

    fn select_cdn(
        &mut self,
        host: &impl Host,
        candidates: &[String],
        purl: &str,
        refresh_ms: u64,
    ) -> Result<Option<String>> {
        let mut bases = Vec::new();
        for candidate in candidates {
            if let Ok(mut url) = Url::parse(candidate) {
                let name = url.host_str().unwrap_or("");
                if !matches!(url.scheme(), "http" | "https")
                    || !(name.ends_with(".qqmusic.qq.com") || name.ends_with(".tc.qq.com"))
                {
                    continue;
                }
                url.set_query(None);
                url.set_fragment(None);
                if !url.path().ends_with('/') {
                    url.set_path(&format!("{}/", url.path()));
                }
                let base = url.to_string();
                if !bases.contains(&base) {
                    bases.push(base);
                }
            }
        }
        if let Some((base, expires)) = &self.cdn {
            if *expires > host.now_ms() && bases.contains(base) {
                return Ok(Some(base.clone()));
            }
        }
        let mut fastest: Option<(String, f64)> = None;
        for base in bases.iter().take(16) {
            let url = Url::parse(base)
                .and_then(|u| u.join(purl))
                .map_err(|_| Error::new("invalidData", "playback.cdn-url"))?;
            let started = host.now_ms();
            let response = host.http(
                "GET",
                url.as_str(),
                vec![
                    ("Accept".into(), "*/*".into()),
                    ("Accept-Encoding".into(), "identity".into()),
                    ("Range".into(), "bytes=0-262143".into()),
                ],
                Vec::new(),
                true,
                0,
            );
            match response {
                Ok(response)
                    if response.status == 206
                        && !response.body.is_empty()
                        && response.body.len() <= 262144 =>
                {
                    let rate = response.body.len() as f64
                        / host.now_ms().saturating_sub(started).max(1) as f64;
                    if fastest.as_ref().is_none_or(|(_, best)| rate > *best) {
                        fastest = Some((base.clone(), rate));
                    }
                }
                Err(error)
                    if matches!(
                        error.code.as_str(),
                        "cancelled" | "staleContext" | "permissionDenied"
                    ) =>
                {
                    return Err(error)
                }
                _ => {}
            }
        }
        let selected = fastest.map(|(base, _)| base);
        if let Some(base) = &selected {
            self.cdn = Some((
                base.clone(),
                host.now_ms()
                    .saturating_add(refresh_ms.clamp(60_000, 86_400_000)),
            ));
        }
        Ok(selected)
    }
}

fn string(value: &Value) -> String {
    model::text(value)
}
fn music_id(credential: &Value) -> String {
    let first = string(&credential["str_musicid"]);
    let second = string(&credential["musicid"]);
    if !first.is_empty() && first != "0" {
        first
    } else if !second.is_empty() && second != "0" {
        second
    } else if !first.is_empty() {
        first
    } else {
        second
    }
}
fn valid_session(value: &Value) -> bool {
    !string(&value["token"]).is_empty()
        && !string(&value["credential"]["musicid"]).is_empty()
        && value["credential"]["musickey"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
        && value["credential"]["loginType"].is_number()
        && device::is_valid(&value["device"])
        && value["expiresAt"].as_f64().is_some_and(f64::is_finite)
}
fn expiry(credential: &Value, now: u64) -> u64 {
    match (
        model::safe_integer(&credential["musickeyCreateTime"]),
        model::safe_integer(&credential["keyExpiresIn"]),
    ) {
        (Some(created), Some(lifetime)) if created > 0 && lifetime > 0 => {
            let ttl = ((created as f64 + lifetime as f64) * 1000.0 - now as f64)
                .clamp(HOUR as f64, (7 * 24 * HOUR) as f64);
            now.saturating_add(ttl as u64)
        }
        _ => now.saturating_add(24 * HOUR),
    }
}
fn credential_from(mut value: Value, default_type: i64) -> Result<Value> {
    if !value.is_object() {
        return Err(Error::new("invalidData", "login.credential"));
    }
    let id = value
        .get("musicid")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| value["str_musicid"].clone());
    if string(&id).is_empty() || string(&value["musickey"]).is_empty() {
        return Err(Error::new("invalidData", "login.credential"));
    }
    value["musicid"] = id;
    let login_type = match value.get("loginType") {
        None => default_type,
        Some(value) => model::safe_integer(value)
            .ok_or_else(|| Error::new("invalidData", "login.credential-type"))?,
    };
    value["loginType"] = json!(login_type);
    Ok(value)
}
fn credential_cookie(credential: &Value) -> String {
    let id = music_id(credential);
    let key = string(&credential["musickey"]);
    format!("uin={id}; qqmusic_uin={id}; qm_keyst={key}; qqmusic_key={key}")
}
fn upstream_error(code: i64, stage: &str) -> Error {
    Error::new(
        if [1000, 104401, 104400].contains(&code) {
            "unauthenticated"
        } else {
            "sourceUnavailable"
        },
        &format!("{stage}.upstream-{code}"),
    )
}
fn safe_upstream(value: &Value) -> String {
    if !value.is_number() && !value.is_string() {
        return "upstream-unknown".into();
    }
    match model::safe_integer(value) {
        Some(value) => format!("upstream-{value}"),
        _ => "upstream-unknown".into(),
    }
}
fn response_json(response: &Response, stage: &str) -> Result<Value> {
    serde_json::from_slice(&response.body).map_err(|_| Error::new("invalidData", stage))
}
fn public_value(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(public_value).collect()),
        Value::Object(items) => Value::Object(
            items
                .into_iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.to_lowercase().as_str(),
                        "musickey"
                            | "authst"
                            | "qqmusic_key"
                            | "token"
                            | "qimei"
                            | "qimei36"
                            | "sid"
                            | "vkey"
                    )
                })
                .map(|(key, value)| (key, public_value(value)))
                .collect(),
        ),
        value => value,
    }
}
fn profile_raw(host: &impl Host, http: &mut HttpClient, session: &Value) -> Result<Value> {
    let credential = &session["credential"];
    let mut profile = json!({"musicid":music_id(credential),"str_musicid":music_id(credential),
        "nickname":string(&credential["nick"]),"nick":string(&credential["nick"]),"avatarUrl":string(&credential["logo"])});
    let user = raw_rpc(
        host,
        http,
        &session["device"],
        Some(credential),
        "music.UserInfo.userInfoServer",
        "GetLoginUserInfo",
        json!({}),
        json!({}),
        0,
    )?;
    if let Some(fields) = public_value(user).as_object() {
        for (key, value) in fields {
            profile[key] = value.clone();
        }
    }
    Ok(profile)
}
#[allow(clippy::too_many_arguments)]
fn raw_rpc(
    host: &impl Host,
    http: &mut HttpClient,
    device: &Value,
    credential: Option<&Value>,
    module: &str,
    method: &str,
    param: Value,
    overrides: Value,
    timeout_ms: u32,
) -> Result<Value> {
    let mut headers = vec![
        ("Content-Type".into(), "application/json".into()),
        (
            "User-Agent".into(),
            format!("QQMusic 14090008(android {})", string(&device["osRelease"])),
        ),
    ];
    if let Some(credential) = credential {
        headers.push(("Cookie".into(), credential_cookie(credential)));
    }
    let payload = json!({"comm":device::comm(device,credential,&overrides),"req_0":{"module":module,"method":method,"param":param}});
    let response = http.request(
        host,
        "POST",
        MUSICU,
        headers,
        payload.to_string().into_bytes(),
        timeout_ms,
    )?;
    let value = response_json(&response, "rpc.json")?;
    let global = model::safe_integer(&value["code"])
        .ok_or_else(|| Error::new("invalidData", "rpc.global-code"))?;
    let item = &value["req_0"];
    let code =
        model::safe_integer(&item["code"]).ok_or_else(|| Error::new("invalidData", "rpc.code"))?;
    if !item.is_object() || global != 0 || code != 0 {
        host.diagnostic(
            "account.rpc",
            "Upstream",
            &format!("upstream-{}", if code != 0 { code } else { global }),
        );
        return Err(if code != 0 {
            upstream_error(code, "rpc")
        } else {
            Error::new("sourceUnavailable", &format!("rpc.global-{global}"))
        });
    }
    Ok(if item["data"].is_object() {
        item["data"].clone()
    } else {
        json!({})
    })
}

fn valid_cookie_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn valid_cookie_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b';' | b',' | b'"' | b'\\'))
}

fn default_cookie_path(url: &Url) -> String {
    let path = url.path();
    if !path.starts_with('/') {
        return "/".into();
    }
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(end) => path[..end].to_owned(),
    }
}

fn cookie_domain_matches(host: &str, cookie: &StoredCookie) -> bool {
    host == cookie.domain
        || (!cookie.host_only
            && host.len() > cookie.domain.len()
            && host.ends_with(&cookie.domain)
            && host.as_bytes()[host.len() - cookie.domain.len() - 1] == b'.')
}

fn cookie_path_matches(request: &str, cookie: &str) -> bool {
    request == cookie
        || (request.starts_with(cookie)
            && (cookie.ends_with('/') || request.as_bytes().get(cookie.len()) == Some(&b'/')))
}

/// A jar is private to one protocol client; WeChat and musicu use separate clients.
#[derive(Default)]
struct HttpClient {
    cookies: BTreeMap<(String, String, String), StoredCookie>,
}
struct StoredCookie {
    value: String,
    domain: String,
    host_only: bool,
    path: String,
    secure: bool,
}
impl HttpClient {
    fn clear(&mut self) {
        self.cookies.clear();
    }
    fn request_cookies<'a>(
        &'a self,
        url: &'a Url,
    ) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        let host = url.host_str().unwrap_or("").to_ascii_lowercase();
        let path = url.path();
        let secure = url.scheme() == "https";
        self.cookies
            .iter()
            .filter(move |(_, cookie)| {
                cookie_domain_matches(&host, cookie)
                    && cookie_path_matches(path, &cookie.path)
                    && (secure || !cookie.secure)
            })
            .map(|((_, _, name), cookie)| (name.as_str(), cookie.value.as_str()))
    }
    fn set_cookie(&mut self, url: &Url, value: &str) -> Result<()> {
        let mut pieces = value.split(';');
        let (name, value) = pieces
            .next()
            .and_then(|pair| pair.split_once('='))
            .map(|(name, value)| (name.trim(), value.trim()))
            .ok_or_else(|| Error::new("invalidData", "http.cookie"))?;
        if !valid_cookie_name(name) || !valid_cookie_value(value) {
            return Err(Error::new("invalidData", "http.cookie"));
        }
        let source = url
            .host_str()
            .ok_or_else(|| Error::new("invalidData", "http.cookie-host"))?
            .to_ascii_lowercase();
        let mut domain = source.clone();
        let mut host_only = true;
        let mut path = default_cookie_path(url);
        let mut secure = false;
        let mut expired = false;
        for attribute in pieces {
            let (key, value) = attribute
                .split_once('=')
                .map(|(key, value)| (key.trim(), value.trim()))
                .unwrap_or((attribute.trim(), ""));
            match key.to_ascii_lowercase().as_str() {
                "domain" => {
                    let candidate = value.trim_start_matches('.').to_ascii_lowercase();
                    if candidate.is_empty()
                        || !(source == candidate || source.ends_with(&format!(".{candidate}")))
                    {
                        continue;
                    }
                    domain = candidate;
                    host_only = false;
                }
                "path" if value.starts_with('/') => path = value.to_owned(),
                "secure" => secure = true,
                "max-age" if value.parse::<i64>().is_ok_and(|seconds| seconds <= 0) => {
                    expired = true
                }
                _ => {}
            }
        }
        let key = (domain.clone(), path.clone(), name.to_owned());
        if expired {
            self.cookies.remove(&key);
        } else {
            self.cookies.insert(
                key,
                StoredCookie {
                    value: value.to_owned(),
                    domain,
                    host_only,
                    path,
                    secure,
                },
            );
        }
        Ok(())
    }
    fn set_explicit_cookie(&mut self, url: &Url, value: &str) -> Result<()> {
        let domain = url
            .host_str()
            .ok_or_else(|| Error::new("invalidData", "http.cookie-host"))?
            .to_ascii_lowercase();
        let path = "/".to_owned();
        for pair in value.split(';') {
            let (name, value) = pair
                .split_once('=')
                .ok_or_else(|| Error::new("invalidData", "http.cookie"))?;
            let (name, value) = (name.trim(), value.trim());
            if !valid_cookie_name(name) || !valid_cookie_value(value) {
                return Err(Error::new("invalidData", "http.cookie"));
            }
            self.cookies.insert(
                (domain.clone(), path.clone(), name.to_owned()),
                StoredCookie {
                    value: value.to_owned(),
                    domain: domain.clone(),
                    host_only: true,
                    path: path.clone(),
                    secure: url.scheme() == "https",
                },
            );
        }
        Ok(())
    }
    fn request(
        &mut self,
        host: &impl Host,
        method: &str,
        url: &str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        timeout_ms: u32,
    ) -> Result<Response> {
        let mut url = Url::parse(url).map_err(|_| Error::new("invalidData", "http.url"))?;
        let mut method = method.to_owned();
        let mut body = body;
        let started = host.now_ms();
        let mut seen = BTreeSet::new();
        seen.insert(url.to_string());
        for (_, value) in headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("cookie"))
        {
            self.set_explicit_cookie(&url, value)?;
        }
        loop {
            let cookies = self.request_cookies(&url).collect::<Vec<_>>();
            let mut request_headers = headers
                .iter()
                .filter(|(name, _)| {
                    !(name.eq_ignore_ascii_case("cookie")
                        || method == "GET" && name.eq_ignore_ascii_case("content-type"))
                })
                .cloned()
                .collect::<Vec<_>>();
            if !cookies.is_empty() {
                request_headers.push((
                    "Cookie".into(),
                    cookies
                        .iter()
                        .map(|(name, value)| format!("{name}={value}"))
                        .collect::<Vec<_>>()
                        .join("; "),
                ));
            }
            let remaining = if timeout_ms == 0 {
                0
            } else {
                let remaining =
                    (timeout_ms as u64).saturating_sub(host.now_ms().saturating_sub(started));
                if remaining == 0 {
                    return Err(Error::new("deadlineExceeded", "http.deadline"));
                }
                remaining as u32
            };
            let response = host.http(
                &method,
                url.as_str(),
                request_headers,
                body.clone(),
                false,
                remaining,
            )?;
            for (_, value) in response
                .headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
            {
                self.set_cookie(&url, value)?;
            }
            let location = response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("location"))
                .map(|(_, value)| value);
            if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                let Some(location) = location else {
                    return Ok(response);
                };
                url = url
                    .join(location)
                    .map_err(|_| Error::new("invalidData", "http.redirect"))?;
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                    return Err(Error::new("invalidData", "http.redirect"));
                }
                if !seen.insert(url.to_string()) {
                    return Err(Error::new("sourceUnavailable", "http.redirect-loop"));
                }
                if response.status == 303
                    || matches!(response.status, 301 | 302) && method == "POST"
                {
                    method = "GET".into();
                    body.clear();
                }
                continue;
            }
            if !(200..400).contains(&response.status) {
                return Err(Error::new(
                    if response.status == 429 {
                        "rateLimited"
                    } else {
                        "network"
                    },
                    "http.status",
                ));
            }
            return Ok(response);
        }
    }
}
