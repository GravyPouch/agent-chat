-- Pure functions for urn:xmpp:ibr-pow:0 (no Prosody runtime state), shared by
-- mod_register_pow and its tests. See docs/design/pow-registration.md §5.
local hashes = require "prosody.util.hashes";

local NS = "urn:xmpp:ibr-pow:0";
local M = { NS = NS };

function M.hex(bytes)
	return (bytes:gsub(".", function (c) return ("%02x"):format(c:byte()); end));
end

function M.unhex(s)
	if #s % 2 ~= 0 or s:find("[^0-9a-fA-F]") then return nil; end
	return (s:gsub("..", function (h) return string.char(tonumber(h, 16)); end));
end

-- tag = hex(HMAC-SHA256(K, NS 0x00 domain 0x00 ipbind 0x00 payload))[1..32]
function M.sign(secret, domain, ipbind, payload)
	return hashes.hmac_sha256(secret, NS.."\0"..domain.."\0"..ipbind.."\0"..payload, true):sub(1, 32);
end

-- token = "1:alg:bits:count:iat:exp:rnd:tag"
function M.parse_token(token)
	if type(token) ~= "string" then return nil; end
	local version, alg, bits, count, iat, exp, rnd, tag =
		token:match("^(%d+):(%w+):(%d+):(%d+):(%d+):(%d+):(%x+):(%x+)$");
	if not version then return nil; end
	return {
		version = version, algorithm = alg,
		bits = tonumber(bits), count = tonumber(count),
		iat = tonumber(iat), exp = tonumber(exp),
		rnd = rnd, tag = tag,
		payload = token:sub(1, #token - #tag - 1),
	};
end

function M.make_payload(alg, bits, count, iat, exp, rnd_hex)
	return table.concat({ "1", alg, tostring(bits), tostring(count), tostring(iat), tostring(exp), rnd_hex }, ":");
end

-- Does the raw digest start with at least `bits` zero bits?
function M.leading_zero_bits_ok(h, bits)
	local full, rem = bits // 8, bits % 8;
	for i = 1, full do
		if h:byte(i) ~= 0 then return false; end
	end
	return rem == 0 or h:byte(full + 1) < (1 << (8 - rem));
end

-- prefix = NS 0x00 token 0x00 domain 0x00 username 0x00
-- H_i = SHA-256(prefix || BYTE(i) || N_i), N_i = 8 bytes (16 hex chars)
function M.verify_solution(token, domain, username, solution, bits, count)
	if type(solution) ~= "string" then return false, "missing solution"; end
	local nonces = {};
	for n in solution:gmatch("[^,]+") do nonces[#nonces + 1] = n; end
	if #nonces ~= count then return false, "expected "..count.." nonces"; end
	local prefix = NS.."\0"..token.."\0"..domain.."\0"..username.."\0";
	for i = 0, count - 1 do
		local n = nonces[i + 1];
		if #n ~= 16 then return false, "nonce "..i.." must be 16 hex characters"; end
		local bytes = M.unhex(n);
		if not bytes then return false, "nonce "..i.." is not hex"; end
		if not M.leading_zero_bits_ok(hashes.sha256(prefix..string.char(i)..bytes), bits) then
			return false, "nonce "..i.." does not satisfy the difficulty";
		end
	end
	return true;
end

return M;
