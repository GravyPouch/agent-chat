# OMEMO interop test bot (python-omemo via slixmpp-omemo). Dev/test only.
FROM python:3.12
RUN pip install --no-cache-dir "slixmpp-omemo>=2,<3"
COPY bot.py /bot.py
VOLUME /data
ENTRYPOINT ["python", "-u", "/bot.py"]
