FROM prosodyim/prosody:13.0

COPY server/deploy/prosody.cfg.lua /etc/prosody/prosody.cfg.lua
COPY server/prosody/ /etc/prosody/custom-modules/
COPY server/deploy/start-prosody.sh /usr/local/bin/start-prosody
RUN chmod +x /usr/local/bin/start-prosody

EXPOSE 5222
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/start-prosody"]
