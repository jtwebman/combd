-- The X clone's tables, for every port and the race's seed. One
-- statement, under a lock: instances starting at once don't trip over
-- each other.
DO $$ BEGIN
PERFORM pg_advisory_xact_lock(7461);
CREATE TABLE IF NOT EXISTS users (
  id bigserial PRIMARY KEY,
  handle text NOT NULL UNIQUE CHECK (handle ~ '^[a-z0-9_]{1,15}$'),
  created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE IF NOT EXISTS posts (
  id bigserial PRIMARY KEY,
  author bigint NOT NULL REFERENCES users,
  body text NOT NULL CHECK (char_length(body) BETWEEN 1 AND 280),
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX IF NOT EXISTS posts_author ON posts (author, id DESC);
CREATE TABLE IF NOT EXISTS follows (
  follower bigint NOT NULL REFERENCES users,
  followee bigint NOT NULL REFERENCES users,
  PRIMARY KEY (follower, followee),
  CHECK (follower <> followee));
CREATE INDEX IF NOT EXISTS follows_followee ON follows (followee);
CREATE TABLE IF NOT EXISTS likes (
  post bigint NOT NULL REFERENCES posts,
  who bigint NOT NULL REFERENCES users,
  PRIMARY KEY (post, who));
END $$;
