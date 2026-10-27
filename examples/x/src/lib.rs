//! An X clone on Postgres (see combd.toml): users, posts, follows, likes,
//! and a home timeline, as a JSON API. No sign-in: a request says who it's
//! from with `as` (a handle).
//!
//! - `POST /api/users {"handle"}`: a user (1 to 15 of a-z, 0-9, _).
//! - `GET /api/users/:handle`: posts, followers, and following counts.
//! - `GET /api/users/:handle/posts?as=&before=`: their posts, newest first.
//! - `POST /api/posts {"as", "body"}`: a post (1 to 280 characters).
//! - `POST` / `DELETE /api/users/:handle/follow {"as"}`: follow, unfollow.
//! - `POST` / `DELETE /api/posts/:id/like {"as"}`: like, unlike.
//! - `GET /api/timeline?as=&before=`: posts by `as` and whom they follow.
//!
//! Lists are 50 posts; `before` (a post id) pages back. A post is
//! `{"id", "author", "body", "at" (ms), "likes", "liked"}` (`liked`: by `as`).

use combd_sdk::{cell, json, pg, Request, Response, Value};

const SCHEMA: &str = include_str!("../schema.sql");

/// A post's columns, as `post` reads them; `$1` is the viewer's id (or null).
const POST: &str = "p.id, u.handle, p.body, (extract(epoch FROM p.created_at) * 1000)::bigint,
  (SELECT count(*) FROM likes l WHERE l.post = p.id),
  EXISTS (SELECT 1 FROM likes l WHERE l.post = p.id AND l.who = $1)
  FROM posts p JOIN users u ON u.id = p.author";

const PAGE: i64 = 50;

/// A home timeline's post ids, `$1`'s and whom they follow, before `$2`:
/// each author's newest from their index, then the newest of those (not a
/// walk down every post looking for theirs).
const TIMELINE: &str = "SELECT t.id FROM (SELECT followee FROM follows WHERE follower = $1 UNION ALL SELECT $1) f (a),
  LATERAL (SELECT id FROM posts WHERE author = f.a AND id < $2 ORDER BY id DESC LIMIT 50) t
  ORDER BY t.id DESC LIMIT 50";

fn handle(req: Request) -> Response {
    route(req).unwrap_or_else(|r| r)
}

fn route(req: Request) -> Result<Response, Response> {
    schema()?;
    let body = req.json();
    let parts: Vec<&str> = req.route().trim_end_matches('/').split('/').skip(2).collect();
    let viewer = || -> Result<Option<i64>, Response> {
        match req.query("as").or(body["as"].as_str()) {
            Some(h) => user_id(h).map(Some),
            None => Ok(None),
        }
    };
    let before = req.query("before").and_then(|b| b.parse::<i64>().ok()).unwrap_or(i64::MAX);
    match (req.method, parts.as_slice()) {
        ("POST", ["users"]) => {
            let h = body["handle"].as_str().unwrap_or("");
            let rows = q("INSERT INTO users (handle) VALUES ($1) RETURNING id", &[json!(h)])?;
            Ok(Response::json(201, &json!({ "id": rows[0][0], "handle": h })))
        }
        ("GET", ["users", h]) => {
            let rows = q(
                "SELECT (SELECT count(*) FROM posts WHERE author = u.id), (SELECT count(*) FROM follows WHERE followee = u.id),
                 (SELECT count(*) FROM follows WHERE follower = u.id) FROM users u WHERE handle = $1",
                &[json!(h)],
            )?;
            let r = rows.first().ok_or_else(|| err(404, "no such user"))?;
            Ok(Response::json(200, &json!({ "handle": h, "posts": r[0], "followers": r[1], "following": r[2] })))
        }
        ("GET", ["users", h, "posts"]) => {
            let author = user_id(h)?;
            let rows = q(&format!("SELECT {POST} WHERE p.author = $2 AND p.id < $3 ORDER BY p.id DESC LIMIT {PAGE}"), &[json!(viewer()?), json!(author), json!(before)])?;
            Ok(posts(rows))
        }
        ("GET", ["timeline"]) => {
            let me = viewer()?.ok_or_else(|| err(400, "timeline: whose? (?as=handle)"))?;
            let rows = q(
                &format!("SELECT {POST} WHERE p.id IN ({TIMELINE}) ORDER BY p.id DESC"),
                &[json!(me), json!(before)],
            )?;
            Ok(posts(rows))
        }
        ("POST", ["posts"]) => {
            let me = viewer()?.ok_or_else(|| err(400, "post as whom? (\"as\")"))?;
            let text = body["body"].as_str().unwrap_or("");
            let rows = q("INSERT INTO posts (author, body) VALUES ($1, $2) RETURNING id, (extract(epoch FROM created_at) * 1000)::bigint", &[json!(me), json!(text)])?;
            Ok(Response::json(201, &json!({ "id": rows[0][0], "body": text, "at": rows[0][1] })))
        }
        (m @ ("POST" | "DELETE"), ["users", h, "follow"]) => {
            let me = viewer()?.ok_or_else(|| err(400, "follow as whom? (\"as\")"))?;
            let them = user_id(h)?;
            if m == "POST" {
                q("INSERT INTO follows (follower, followee) VALUES ($1, $2) ON CONFLICT DO NOTHING", &[json!(me), json!(them)])?;
            } else {
                q("DELETE FROM follows WHERE follower = $1 AND followee = $2", &[json!(me), json!(them)])?;
            }
            Ok(Response::json(200, &json!({ "following": m == "POST" })))
        }
        (m @ ("POST" | "DELETE"), ["posts", id, "like"]) => {
            let me = viewer()?.ok_or_else(|| err(400, "like as whom? (\"as\")"))?;
            let id: i64 = id.parse().map_err(|_| err(404, "no such post"))?;
            if m == "POST" {
                q("INSERT INTO likes (post, who) VALUES ($1, $2) ON CONFLICT DO NOTHING", &[json!(id), json!(me)])?;
            } else {
                q("DELETE FROM likes WHERE post = $1 AND who = $2", &[json!(id), json!(me)])?;
            }
            let n = q("SELECT count(*) FROM likes WHERE post = $1", &[json!(id)])?;
            Ok(Response::json(200, &json!({ "liked": m == "POST", "likes": n[0][0] })))
        }
        _ => Err(err(404, "not found")),
    }
}

/// The tables, once per instance (`IF NOT EXISTS`, under a lock: instances
/// starting at once don't trip over each other).
fn schema() -> Result<(), Response> {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !DONE.load(std::sync::atomic::Ordering::Relaxed) {
        q(SCHEMA, &[])?;
        DONE.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(())
}

fn user_id(handle: &str) -> Result<i64, Response> {
    let rows = q("SELECT id FROM users WHERE handle = $1", &[json!(handle)])?;
    rows.first().and_then(|r| r[0].as_i64()).ok_or_else(|| err(404, "no such user"))
}

fn posts(rows: Vec<Vec<Value>>) -> Response {
    let list: Vec<Value> = rows.iter().map(|r| json!({ "id": r[0], "author": r[1], "body": r[2], "at": r[3], "likes": r[4], "liked": r[5] })).collect();
    Response::json(200, &json!({ "posts": list }))
}

/// A statement on the pool; what went wrong as a status (a broken rule is
/// the request's fault, anything else the server's, and not explained).
fn q(sql: &str, params: &[Value]) -> Result<Vec<Vec<Value>>, Response> {
    pg::query("main", sql, params).map_err(|e| match e.get(..5) {
        Some("23505") => err(409, "already taken"),
        Some("23503") => err(404, "no such post or user"),
        Some("23514" | "22001") => err(400, "not allowed: a handle is 1 to 15 of a-z, 0-9 and _; a post 1 to 280 characters; no following yourself"),
        _ => err(500, "database error"),
    })
}

fn err(status: u16, msg: &str) -> Response {
    Response::json(status, &json!({ "error": msg }))
}

cell!(handle);
