-- Checks pow.lib.lua against the test vector in docs/design/pow-registration.md §5.7.
-- Run inside the Prosody container:
--   lua5.4 /etc/prosody/custom-modules/mod_register_pow/test_vector.lua
local here = arg[0]:match("^(.*)/[^/]*$") or ".";
package.path = "/usr/share/lua/5.4/?.lua;"..package.path;
package.cpath = "/usr/lib/aarch64-linux-gnu/lua/5.4/?.so;/usr/lib/x86_64-linux-gnu/lua/5.4/?.so;"..package.cpath;
local pow = dofile(here.."/pow.lib.lua");

local K = "example-secret-do-not-use";
local domain, ipbind, username = "example.org", "203.0.113.7/32", "agent-7k2m9qx4td";
local token = "1:sha256:25:8:1794326400:1794330000:9f2c4e7a1b3d5f60718293a4b5c6d7e8:6b8ae478ff3592e715741262396837c1";
local solution = "0000000001dee49f,000000000212fd53,00000000006093ad,000000000016321f,"..
	"00000000002aa9f1,0000000003f1a352,00000000022ca4ce,000000000014a0b1";

local t = assert(pow.parse_token(token), "token parses");
assert(t.bits == 25 and t.count == 8, "parameters");
assert(pow.sign(K, domain, ipbind, t.payload) == t.tag, "tag matches");
assert(pow.sign(K, domain, "198.51.100.1/32", t.payload) ~= t.tag, "tag is bound to the IP prefix");
assert(pow.verify_solution(token, domain, username, solution, t.bits, t.count), "solution verifies");
assert(not pow.verify_solution(token, domain, "agent-7k2m9qx4tX", solution, t.bits, t.count), "bound to username");
assert(not pow.verify_solution(token, "example.net", username, solution, t.bits, t.count), "bound to domain");
assert(not pow.verify_solution(token, domain, username, solution:sub(18), t.bits, t.count), "needs all nonces");
assert(pow.leading_zero_bits_ok("\0\0\1", 23) and not pow.leading_zero_bits_ok("\0\0\1", 24), "bit check");
print("mod_register_pow test vector: OK");
