#!/usr/bin/env bash
# Smoke-tests a Nodal server image end to end: health, web viewer, auth, upload → tiling → serving.
#   scripts/smoke-test-docker.sh IMAGE
set -euo pipefail
IMAGE=${1:?usage: smoke-test-docker.sh IMAGE}
PORT=${PORT:-18080}
NAME=nodal-smoke-$$
TOKEN=ci-token
B=http://127.0.0.1:$PORT
PANO=$(mktemp -d)/smoke-test.png

cleanup() {
  status=$?
  if [ $status -ne 0 ]; then echo "--- container log ---"; docker logs "$NAME" 2>&1 | tail -40 || true; fi
  docker rm -f -v "$NAME" >/dev/null 2>&1 || true
  rm -rf "$(dirname "$PANO")"
  exit $status
}
trap cleanup EXIT
json() { python3 -c "import sys, json; print(json.load(sys.stdin)$1)"; }

docker run -d --name "$NAME" -p "$PORT:8080" -e NODAL_TOKEN=$TOKEN "$IMAGE" >/dev/null
for _ in $(seq 1 60); do curl -fsS "$B/healthz" >/dev/null 2>&1 && break; sleep 0.5; done
echo "health:      $(curl -fsS "$B/healthz")"
docker exec "$NAME" /usr/local/bin/nodal-server healthcheck && echo "healthcheck: ok"
curl -fsS "$B/api/config" | grep -q '"auth_required":true' && echo "auth:        required"
curl -fsS "$B/" | grep -q nodal_canvas && echo "viewer:      index.html served"
curl -fsS -o /dev/null -H 'Accept-Encoding: br' "$B/pkg/nodal_app_bg.wasm" && echo "viewer:      wasm served"

python3 "$(dirname "$0")/make-test-pano.py" "$PANO" 1024
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary @"$PANO" "$B/api/tours")
[ "$code" = 401 ] && echo "upload:      rejected without token (401)"
job=$(curl -fsS -X POST -H "Authorization: Bearer $TOKEN" -H "X-Filename: smoke-test.png" -H "X-Title: Smoke Test" \
  --data-binary @"$PANO" "$B/api/tours" | json '["id"]')
state=queued
for _ in $(seq 1 240); do
  state=$(curl -fsS "$B/api/jobs/$job" | json '["state"]')
  [ "$state" = "done" ] || [ "$state" = "failed" ] && break
  sleep 0.5
done
[ "$state" = "done" ] || { echo "tiling job ended as: $state"; curl -fsS "$B/api/jobs/$job"; exit 1; }
echo "tiling:      done"
curl -fsS "$B/api/tours" | grep -q '"id":"smoke-test"' && echo "library:     tour listed"
curl -fsS "$B/tours/smoke-test/tour.json" | grep -q '"tile_size"' && echo "tour.json:   served"
ctype=$(curl -fsS -o /dev/null -w '%{content_type}' "$B/tours/smoke-test/scenes/smoke-test/thumb.jpg")
[ "$ctype" = image/jpeg ] && echo "thumbnail:   served ($ctype)"
echo "smoke test passed"
