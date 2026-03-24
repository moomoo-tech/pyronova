-- wrk POST script: reads body from env var WRK_BODY or file
wrk.method = "POST"
wrk.headers["Content-Type"] = "application/json"

local body_file = os.getenv("WRK_BODY_FILE")
if body_file then
    local f = io.open(body_file, "r")
    if f then
        wrk.body = f:read("*all")
        f:close()
    end
end
