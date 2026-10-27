-- wrk's requests for the X race: random users, 80% home timelines, 10% new
-- posts, 8% likes (of any of the seed's posts), 2% follows.
-- wrk -s bench/x/load.lua -H "Host: x.localhost" http://srv:8080
local users, posts = 10000, 200000
local json = { ["content-type"] = "application/json" }

function setup(thread)
  thread:set("seed", math.random(1, 1e9))
end

function init()
  math.randomseed(seed)
end

function request()
  local me = "u" .. math.random(0, users - 1)
  local r = math.random(100)
  if r <= 80 then
    return wrk.format("GET", "/api/timeline?as=" .. me)
  elseif r <= 90 then
    return wrk.format("POST", "/api/posts", json, '{"as":"' .. me .. '","body":"hello from ' .. me .. '"}')
  elseif r <= 98 then
    return wrk.format("POST", "/api/posts/" .. math.random(1, posts) .. "/like", json, '{"as":"' .. me .. '"}')
  else
    return wrk.format("POST", "/api/users/u" .. math.random(0, users - 1) .. "/follow", json, '{"as":"' .. me .. '"}')
  end
end
