"""OMEMO interop test bot (independent implementation: python-omemo via slixmpp-omemo).

Echoes every encrypted 1:1 message back, encrypted, prefixed with the OMEMO
namespace it arrived in. Optionally sends the first message (GREET_JID) so
the other side receives a key exchange.

Environment:
  BOT_JID, BOT_PASSWORD   account
  XMPP_HOST, XMPP_PORT    server (plaintext; dev only)
  OMEMO_BACKENDS          both | twomemo | oldmemo
  GREET_JID               optional: JID to greet once on start
"""

import ast
import asyncio
import json
import secrets
import ssl
import xml.etree.ElementTree as ET
from xml.sax.saxutils import escape
import logging
import os
from pathlib import Path
from typing import Any, Dict, FrozenSet, Optional

import oldmemo
import twomemo
from omemo.storage import Just, Maybe, Nothing, Storage
from omemo.types import DeviceInformation, JSONType
from slixmpp.clientxmpp import ClientXMPP
from slixmpp.jid import JID
from slixmpp.plugins import register_plugin
from slixmpp.stanza import Message
from slixmpp.xmlstream.handler import CoroutineCallback
from slixmpp.xmlstream.matcher import MatchXPath

import slixmpp_omemo.xep_0384 as x384
from slixmpp_omemo import TrustLevel, XEP_0384

log = logging.getLogger("omemo-bot")
BACKENDS = os.environ.get("OMEMO_BACKENDS", "both")


async def _prepare(xmpp, xep_0384, storage, legacy_storage, signed_pre_key_rotation_period=7 * 24 * 3600,
                   pre_key_refill_threshold=99, max_num_per_session_skipped_keys=1000,
                   max_num_per_message_skipped_keys=None):
    backends = []
    if BACKENDS in ("both", "twomemo"):
        backends.append(twomemo.Twomemo(storage, max_num_per_session_skipped_keys, max_num_per_message_skipped_keys))
    if BACKENDS in ("both", "oldmemo"):
        backends.append(oldmemo.Oldmemo(storage, max_num_per_session_skipped_keys, max_num_per_message_skipped_keys))
    sm = await x384._make_session_manager(xmpp, xep_0384).create(
        backends, storage, xmpp.boundjid.bare,
        initial_own_label=None,
        undecided_trust_level_name=TrustLevel.UNDECIDED.value,
        signed_pre_key_rotation_period=signed_pre_key_rotation_period,
        pre_key_refill_threshold=pre_key_refill_threshold,
    )
    # slixmpp-omemo refreshes device lists for every namespace it knows;
    # skip the ones whose backend we deliberately didn't load.
    loaded = {b.namespace for b in backends}
    for name in ("update_device_list", "refresh_device_list"):
        orig = getattr(sm, name)

        def make(orig):
            async def wrapper(namespace, *args, **kwargs):
                if namespace not in loaded:
                    return None
                return await orig(namespace, *args, **kwargs)
            return wrapper

        setattr(sm, name, make(orig))
    await sm.ensure_data_consistency()
    await sm.after_history_sync()
    return sm


x384._prepare = _prepare  # restrict backends for single-version tests


class StorageImpl(Storage):
    def __init__(self, path: Path) -> None:
        super().__init__()
        self.path = path
        self.data: Dict[str, JSONType] = {}
        if path.exists():
            self.data = json.loads(path.read_text("utf8"))

    async def _load(self, key: str) -> Maybe[JSONType]:
        return Just(self.data[key]) if key in self.data else Nothing()

    async def _store(self, key: str, value: JSONType) -> None:
        self.data[key] = value
        self.path.write_text(json.dumps(self.data), "utf8")

    async def _delete(self, key: str) -> None:
        self.data.pop(key, None)
        self.path.write_text(json.dumps(self.data), "utf8")


class XEP_0384Impl(XEP_0384):
    default_config = {"fallback_message": "This message is OMEMO encrypted.", "json_file_path": None}

    def plugin_init(self) -> None:
        self._storage_impl = StorageImpl(Path(self.json_file_path))
        super().plugin_init()

    @property
    def storage(self) -> Storage:
        return self._storage_impl

    @property
    def _btbv_enabled(self) -> bool:
        return True

    async def _devices_blindly_trusted(self, blindly_trusted: FrozenSet[DeviceInformation], identifier: Optional[str]) -> None:
        log.info("blindly trusted: %s", [(d.bare_jid, d.device_id, sorted(d.namespaces)) for d in blindly_trusted])

    async def _prompt_manual_trust(self, manually_trusted: FrozenSet[DeviceInformation], identifier: Optional[str]) -> None:
        sm = await self.get_session_manager()
        for d in manually_trusted:
            await sm.set_trust(d.bare_jid, d.identity_key, TrustLevel.TRUSTED.value)


register_plugin(XEP_0384Impl)


class Bot(ClientXMPP):
    def __init__(self, jid: str, password: str) -> None:
        super().__init__(jid, password)
        # Dev server only: Prosody's self-signed localhost certificate.
        self.ssl_context.check_hostname = False
        self.ssl_context.verify_mode = ssl.CERT_NONE
        self.add_event_handler("session_start", self.start)
        self.register_handler(CoroutineCallback("Messages", MatchXPath(f"{{{self.default_ns}}}message"), self.on_message))

    async def start(self, _event: Any) -> None:
        self.send_presence()
        await self.get_roster()
        sm = await self.plugin["xep_0384"].get_session_manager()
        log.info("READY backends=%s own devices=%s", BACKENDS, await sm.get_own_device_information())
        greet = os.environ.get("GREET_JID")
        if greet:
            await asyncio.sleep(2)
            await self.reply(JID(greet), f"hello from {self.boundjid.bare} ({BACKENDS})")

    async def reply(self, to: JID, text: str) -> None:
        xep = self.plugin["xep_0384"]
        if BACKENDS == "twomemo":
            # slixmpp-omemo has no SCE support, so build the XEP-0420 envelope
            # ourselves and drive python-omemo's twomemo backend directly.
            await xep.refresh_device_lists({JID(to.bare)})
            sm = await xep.get_session_manager()
            envelope = (
                "<envelope xmlns='urn:xmpp:sce:1'><content>"
                f"<body xmlns='jabber:client'>{escape(text)}</body></content>"
                f"<rpad>{secrets.token_hex(secrets.randbelow(40))}</rpad>"
                f"<from jid='{self.boundjid.bare}'/></envelope>"
            ).encode()
            messages, errors = await sm.encrypt(
                frozenset({to.bare}),
                {twomemo.twomemo.NAMESPACE: envelope},
                backend_priority_order=[twomemo.twomemo.NAMESPACE],
            )
            if errors:
                log.warning("encryption errors: %s", errors)
            stanza = self.make_message(mto=to, mtype="chat")
            stanza["body"] = "This message is OMEMO encrypted."
            stanza.enable("store")
            for m in messages:
                stanza.append(twomemo.etree.serialize_message(m))
            stanza.send()
            log.info("SENT encrypted (omemo:2/SCE) to %s: %s", to.bare, text)
            return
        msg = self.make_message(mto=to, mtype="chat")
        msg["body"] = text
        msg.set_from(self.boundjid)
        encrypted, errors = await xep.encrypt_message(msg, {JID(to.bare)})
        if errors:
            log.warning("encryption errors: %s", errors)
        if encrypted is None:
            log.error("nothing encrypted for %s", to)
            return
        encrypted.send()
        log.info("SENT encrypted to %s: %s", to.bare, text)

    async def on_message(self, stanza: Message) -> None:
        if stanza["type"] not in ("chat", "normal"):
            return
        xep = self.plugin["xep_0384"]
        namespaces = xep.is_encrypted(stanza)
        if not namespaces:
            if stanza["body"]:
                log.info("PLAINTEXT from %s: %s", stanza["from"], stanza["body"])
            return
        try:
            message, info = await xep.decrypt_message(stanza)
        except NotImplementedError as e:
            # Ratchet + payload decryption succeeded; slixmpp-omemo just can't
            # unpack SCE. Unpack the envelope ourselves.
            raw = str(e).split("Plaintext: ", 1)[1]
            if raw == "None":
                log.info("empty OMEMO message from %s (['urn:xmpp:omemo:2'])", stanza["from"])
                return
            env = ET.fromstring(ast.literal_eval(raw))
            ns = {"sce": "urn:xmpp:sce:1", "c": "jabber:client"}
            body = env.findtext("sce:content/c:body", namespaces=ns)
            env_from = env.find("sce:from", ns)
            log.info("DECRYPTED from %s via ['urn:xmpp:omemo:2'] (SCE from=%s): %s", stanza["from"],
                     env_from.get("jid") if env_from is not None else None, body)
            if body:
                await self.reply(JID(stanza["from"].bare), f"echo[urn:xmpp:omemo:2]: {body}")
            return
        except Exception as e:  # noqa: BLE001
            log.exception("DECRYPT FAILED from %s", stanza["from"])
            m = self.make_message(mto=stanza["from"].bare, mtype="chat")
            m["body"] = f"bot could not decrypt: {type(e).__name__}: {e}"
            m.send()
            return
        body = message["body"]
        if not body:
            log.info("empty OMEMO message from %s (%s)", stanza["from"], sorted(namespaces))
            return
        log.info("DECRYPTED from %s via %s: %s", stanza["from"], sorted(namespaces), body)
        await self.reply(JID(stanza["from"].bare), f"echo[{sorted(namespaces)[0]}]: {body}")


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(name)s %(levelname)s %(message)s")
    bot = Bot(os.environ["BOT_JID"], os.environ["BOT_PASSWORD"])
    bot.register_plugin("xep_0199")
    bot.register_plugin("xep_0380")
    bot.register_plugin("xep_0384", {"json_file_path": os.environ.get("BOT_STORE", "/data/omemo.json")}, module=__name__)
    bot.connect(os.environ.get("XMPP_HOST", "prosody"), int(os.environ.get("XMPP_PORT", "5222")))
    asyncio.get_event_loop().run_forever()


if __name__ == "__main__":
    main()
