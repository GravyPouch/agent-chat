-- mod_register_pow: proof-of-work gate for in-band registration (XEP-0077).
--
-- Clients fetch the registration form, which carries a stateless HMAC-signed
-- challenge, solve a SHA-256 puzzle bound to the server, the client's IP
-- prefix and the chosen username, and submit the solution with the form.
-- Valid submissions fall through to mod_register_ibr, which creates the
-- account as usual. Protocol: urn:xmpp:ibr-pow:0 (provisional), see
-- docs/design/pow-registration.md in the agent-chat repository.
--
-- Copyright 2026 agent-chat contributors. GPL-3.0-or-later.

local st = require "prosody.util.stanza";
local dataforms = require "prosody.util.dataforms";
local random = require "prosody.util.random";
local ip_util = require "prosody.util.ip";
local errors = require "prosody.util.error";
local cache = require "prosody.util.cache";
local datetime = require "prosody.util.datetime";
local hashes = require "prosody.util.hashes";
local nodeprep = require "prosody.util.encodings".stringprep.nodeprep;

local pow = module:require("pow");
local NS = pow.NS;
local function F(name) return "{"..NS.."}"..name; end

module:depends("register_ibr");

local base_bits = module:get_option_integer("register_pow_bits", 25, 1, 48);
local count = module:get_option_integer("register_pow_count", 8, 1, 64);
local max_bits = module:get_option_integer("register_pow_max_bits", 31, base_bits, 56);
local ttl = module:get_option_period("register_pow_ttl", "1h");
local secret = module:get_option_string("register_pow_secret") or random.bytes(32);
local bind_ip = module:get_option_boolean("register_pow_bind_ip", true);
local v4_len = module:get_option_integer("register_pow_ipv4_prefix", 32, 8, 32);
local v6_len = module:get_option_integer("register_pow_ipv6_prefix", 56, 8, 128);
local prefix_window = module:get_option_period("register_pow_prefix_window", "24h");
local prefix_cap = module:get_option_integer("register_pow_prefix_escalation_cap", 6, 0, 32);
local load_threshold = module:get_option_integer("register_pow_load_threshold", 30, 1);
local load_cap = module:get_option_integer("register_pow_load_escalation_cap", 4, 0, 32);
local exempt_invites = module:get_option_boolean("register_pow_exempt_invites", true);
local require_encryption = module:get_option_boolean("c2s_require_encryption",
	module:get_option_boolean("require_encryption", true));
local title = module:get_option_string("registration_title", "Creating a new account");
local form_instructions = module:get_option_string("registration_instructions",
	"Choose a username and password. Your client must also solve the proof-of-work challenge.");
local legacy_instructions = module:get_option_string("register_pow_instructions",
	"This server requires a proof of work to register. Use a client that supports "..NS..
	", or ask the administrator for an invitation.");

if v4_len % 8 ~= 0 or v6_len % 8 ~= 0 then
	module:log("warn", "IP prefix lengths must be multiples of 8; using /32 and /56");
	v4_len, v6_len = 32, 56;
end

-- Tokens are marked spent when an account is created with them.
local spent = cache.new(module:get_option_integer("register_pow_replay_cache_size", 10000, 100));
-- Recent successful PoW registrations, for difficulty escalation.
local by_prefix = {};   -- prefix -> { timestamps }
local recent_all = {};  -- timestamps within the last hour

local registration_form = dataforms.new{
	title = title;
	instructions = form_instructions;
	{ name = "FORM_TYPE", type = "hidden", value = "jabber:iq:register" };
	{ name = "username", type = "text-single", label = "Username", required = true };
	{ name = "password", type = "text-private", label = "Password", required = true };
	{ name = F"challenge", type = "hidden" };
	{ name = F"algorithm", type = "hidden" };
	{ name = F"bits", type = "hidden" };
	{ name = F"count", type = "hidden" };
	{ name = F"expires", type = "hidden" };
	{ name = F"solution", type = "text-single", required = true,
		label = "Proof of work (computed automatically by a supporting client)" };
};

-- "203.0.113.7/32", "2001:db8:1200::/56"; IPv4-mapped IPv6 counts as IPv4.
local function prefix_of(session)
	local addr = session.ip;
	if not addr then return nil; end
	local mapped = addr:match("^::[fF][fF][fF][fF]:(%d+%.%d+%.%d+%.%d+)$");
	local ip = ip_util.new_ip(mapped or addr);
	if not ip then return nil; end
	local len = ip.proto == "IPv6" and v6_len or v4_len;
	return ip_util.truncate(ip, len).normal.."/"..len;
end

local function ipbind(session)
	if not bind_ip then return ""; end
	return prefix_of(session) or "";
end

local function prune(list, since)
	local kept = {};
	for _, t in ipairs(list) do
		if t >= since then kept[#kept + 1] = t; end
	end
	return kept;
end

local function prefix_extra(prefix, now)
	if not prefix then return 0; end
	local list = prune(by_prefix[prefix] or {}, now - prefix_window);
	by_prefix[prefix] = #list > 0 and list or nil;
	return math.min(prefix_cap, #list);
end

local function load_extra(now)
	recent_all = prune(recent_all, now - 3600);
	local rate = #recent_all;
	if rate < load_threshold then return 0; end
	return math.min(load_cap, math.floor(math.log(rate / load_threshold, 2)) + 1);
end

local function required_bits(session, now)
	return math.min(max_bits, base_bits + prefix_extra(prefix_of(session), now));
end

local function pow_error(condition, text)
	return errors.new({
		type = "modify", condition = "not-acceptable", text = text,
		extra = { namespace = NS, condition = condition },
	});
end

local function issue_challenge(session)
	local now = os.time();
	local bits = math.min(max_bits, required_bits(session, now) + load_extra(now));
	local exp = now + ttl;
	local payload = pow.make_payload("sha256", bits, count, now, exp, pow.hex(random.bytes(16)));
	local token = payload..":"..pow.sign(secret, module.host, ipbind(session), payload);
	module:log("debug", "Issued %d-bit challenge to %s", bits, prefix_of(session) or "?");
	return {
		[F"challenge"] = token,
		[F"algorithm"] = "sha256",
		[F"bits"] = tostring(bits),
		[F"count"] = tostring(count),
		[F"expires"] = datetime.datetime(exp),
	};
end

local function field_value(form, var)
	for field in form:childtags("field") do
		if field.attr.var == var then
			return field:get_child_text("value");
		end
	end
end

module:hook("stream-features", function (event)
	local session = event.origin;
	if session.type ~= "c2s_unauthed" or (require_encryption and not session.secure) then return; end
	event.features:tag("register", { xmlns = NS }):up();
end);

-- Runs before mod_register_ibr (priority 0).
module:hook("stanza/iq/jabber:iq:register:query", function (event)
	local session, stanza = event.origin, event.stanza;
	if session.type ~= "c2s_unauthed" or (require_encryption and not session.secure) then
		return; -- mod_register_ibr answers with its usual error
	end

	if stanza.attr.type == "get" then
		local query = st.stanza("query", { xmlns = "jabber:iq:register" })
			:text_tag("instructions", legacy_instructions)
			:add_child(registration_form:form(issue_challenge(session)));
		session.send(st.reply(stanza):add_child(query));
		return true;
	end

	if stanza.attr.type ~= "set" then return; end
	if exempt_invites and session.validated_invite then return; end -- XEP-0445 invite

	local function fail(condition, text)
		module:log("debug", "Rejected registration from %s: %s", prefix_of(session) or "?", condition);
		session.send(st.error_reply(stanza, pow_error(condition, text)));
		return true;
	end

	local query = stanza.tags[1];
	local form = query and query:get_child("x", "jabber:x:data");
	if not form then
		return fail("pow-required", "This server requires a proof of work; request the registration form.");
	end
	local username = field_value(form, "username");
	local token = field_value(form, F"challenge");
	local solution = field_value(form, F"solution");
	if not token or not solution then
		return fail("pow-required", "Missing proof-of-work challenge or solution.");
	end

	local t = pow.parse_token(token);
	if not t or t.version ~= "1" then
		return fail("challenge-invalid", "Unknown proof-of-work challenge format.");
	end
	if t.algorithm ~= "sha256" then
		return fail("unsupported-algorithm", "Unsupported proof-of-work algorithm.");
	end
	local now = os.time();
	if now > t.exp or t.iat > now + 60 then
		return fail("challenge-expired", "Proof-of-work challenge expired; request a new registration form.");
	end
	if not hashes.equals(pow.sign(secret, module.host, ipbind(session), t.payload), t.tag) then
		return fail("challenge-invalid", "Proof-of-work challenge is invalid for this server or address.");
	end
	if spent:get(t.tag) then
		return fail("challenge-used", "This proof-of-work challenge was already used.");
	end
	if t.bits < required_bits(session, now) then
		return fail("challenge-outdated", "The required difficulty went up; request a new registration form.");
	end
	local user = username and nodeprep(username, true);
	if not user or #user == 0 then
		session.send(st.error_reply(stanza, "modify", "not-acceptable", "The requested username is invalid."));
		return true;
	end
	local ok, why = pow.verify_solution(token, module.host, user, solution, t.bits, t.count);
	if not ok then
		return fail("solution-invalid", "Proof of work is not valid: "..why);
	end

	session.ibr_pow = { username = user, tag = t.tag, exp = t.exp, bits = t.bits };
	-- Fall through: mod_register_ibr creates the account.
end, 10);

-- Second line of defence: every in-band registration on this host needs a
-- verified proof of work (or an invite) for exactly this username.
module:hook("user-registering", function (event)
	local session = event.session;
	if not session or session.type ~= "c2s_unauthed" then return; end
	if exempt_invites and session.validated_invite then return; end
	if not (session.ibr_pow and session.ibr_pow.username == event.username) then
		event.allowed = false;
		event.error = pow_error("pow-required", "A proof of work is required to register.");
	end
end, 10);

module:hook("user-registered", function (event)
	local session = event.session;
	local proof = session and session.ibr_pow;
	if not proof or proof.username ~= event.username then return; end
	session.ibr_pow = nil;
	spent:set(proof.tag, proof.exp);
	local now = os.time();
	local prefix = prefix_of(session);
	if prefix then
		local list = by_prefix[prefix] or {};
		list[#list + 1] = now;
		by_prefix[prefix] = list;
	end
	recent_all[#recent_all + 1] = now;
	module:log("info", "Account %s registered with a %d-bit proof of work from %s",
		event.username, proof.bits, prefix or "?");
end);
