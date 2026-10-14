-- POST /do/roomN/incr across 1,000 objects.
math.randomseed(os.time())
request = function()
  return wrk.format("POST", "/do/room" .. math.random(1, 1000) .. "/incr")
end
