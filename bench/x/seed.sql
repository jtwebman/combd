-- The race's data, the same for every server: 10,000 users (u0 to u9999),
-- each following 50 others, 200,000 posts over the last ~2 days, and
-- 500,000 likes. Run after examples/x/schema.sql; the same every time
-- (setseed).
SELECT setseed(0.42);
INSERT INTO users (handle) SELECT 'u' || i FROM generate_series(0, 9999) i;
INSERT INTO follows (follower, followee)
  SELECT u, 1 + (u + floor(random() * 9999)::bigint) % 10000
  FROM generate_series(1, 10000) u, generate_series(1, 50)
  ON CONFLICT DO NOTHING;
INSERT INTO posts (author, body, created_at)
  SELECT 1 + floor(random() * 10000)::bigint, 'post number ' || g, now() - (200000 - g) * interval '1 second'
  FROM generate_series(1, 200000) g;
INSERT INTO likes (post, who)
  SELECT 1 + floor(random() * 200000)::bigint, 1 + floor(random() * 10000)::bigint
  FROM generate_series(1, 500000)
  ON CONFLICT DO NOTHING;
ANALYZE;
