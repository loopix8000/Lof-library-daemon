FROM python:3.12-slim

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    LOF_DATA_DIR=/data

WORKDIR /app
COPY pyproject.toml ./
COPY requirements.lock ./
COPY lof_daemon ./lof_daemon
RUN pip install --no-cache-dir -r requirements.lock \
    && pip install --no-cache-dir --no-deps . \
    && groupadd --gid 10001 lof \
    && useradd --uid 10001 --gid lof --no-create-home lof \
    && mkdir /data && chown lof:lof /data

USER lof
VOLUME ["/data"]
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
    CMD python -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8787/healthz', timeout=3)"
CMD ["python", "-m", "lof_daemon"]
