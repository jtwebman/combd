#!/bin/sh
# Every route once, checked: ./check.sh [base URL] (default: combd dev's).
# New users each run (a_<n>, b_<n>), so it runs against the same database
# again and again.
set -eu
B=${1:-http://x.localhost:8080}
n=$(($(date +%s) % 100000))$$
a=a_$n b=b_$n
fail() { echo "check: $1" >&2; exit 1; }
# want <status> <method> <path> [body] [grep pattern]: the answer, checked.
want() {
    out=$(curl -s -w '\n%{http_code}' -X "$2" "$B$3" ${4:+-H 'content-type: application/json' -d "$4"}) || fail "$2 $3: no answer"
    code=$(printf '%s\n' "$out" | tail -n 1) body=$(printf '%s\n' "$out" | sed '$d')
    [ "$code" = "$1" ] || fail "$2 $3: want $1, got $code: $body"
    [ -z "${5:-}" ] || printf %s "$body" | grep -q -- "$5" || fail "$2 $3: want /$5/ in $body"
    printf %s "$body"
}
want 201 POST /api/users "{\"handle\":\"$a\"}" >/dev/null
want 201 POST /api/users "{\"handle\":\"$b\"}" >/dev/null
want 409 POST /api/users "{\"handle\":\"$a\"}" >/dev/null
want 400 POST /api/users '{"handle":"Not OK"}' >/dev/null
p=$(want 201 POST /api/posts "{\"as\":\"$b\",\"body\":\"hello from b\"}" | sed 's/.*"id":\([0-9]*\).*/\1/')
want 400 POST /api/posts "{\"as\":\"$b\",\"body\":\"\"}" >/dev/null
want 404 POST /api/posts '{"as":"nobody_here","body":"hi"}' >/dev/null
want 200 GET "/api/timeline?as=$a" '' '"posts":\[\]' >/dev/null
want 200 POST "/api/users/$b/follow" "{\"as\":\"$a\"}" '"following":true' >/dev/null
want 400 POST "/api/users/$a/follow" "{\"as\":\"$a\"}" >/dev/null
want 200 GET "/api/timeline?as=$a" '' "\"id\":$p" >/dev/null
want 200 POST "/api/posts/$p/like" "{\"as\":\"$a\"}" '"likes":1' >/dev/null
want 200 POST "/api/posts/$p/like" "{\"as\":\"$a\"}" '"likes":1' >/dev/null
want 200 GET "/api/users/$b/posts?as=$a" '' '"liked":true' >/dev/null
want 200 DELETE "/api/posts/$p/like" "{\"as\":\"$a\"}" '"likes":0' >/dev/null
want 404 POST /api/posts/999999999999/like "{\"as\":\"$a\"}" >/dev/null
want 200 GET "/api/users/$b" '' '"followers":1' >/dev/null
want 200 DELETE "/api/users/$b/follow" "{\"as\":\"$a\"}" '"following":false' >/dev/null
want 200 GET "/api/timeline?as=$a" '' '"posts":\[\]' >/dev/null
want 200 GET "/api/timeline?as=$b&before=$p" '' '"posts":\[\]' >/dev/null
want 404 GET /api/users/nobody_here >/dev/null
echo "check: ok"
