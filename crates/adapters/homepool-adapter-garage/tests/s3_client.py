"""Test-only S3 SigV4 client, using Python's standard library.

All inputs, including generated credentials, arrive over stdin. Never print them.
The caller runs each request in a separate container sharing Garage's namespace.
"""
import datetime
import hashlib
import hmac
import http.client
import json
import sys
import urllib.parse


def sign(key, message):
    return hmac.new(key, message.encode(), hashlib.sha256).digest()


def run():
    request = json.load(sys.stdin)
    action = request["action"]
    method = "PUT" if action in ("put", "deny-put", "deny-create") else "GET"
    payload = b"homepool shared storage\n" if action in ("put", "deny-put") else b""
    path = "/" + urllib.parse.quote(request["bucket"], safe="")
    if action != "deny-create":
        path += "/sample"
    now = datetime.datetime.now(datetime.timezone.utc)
    date = now.strftime("%Y%m%d")
    timestamp = now.strftime("%Y%m%dT%H%M%SZ")
    host = request["host"] + ":3900"
    digest = hashlib.sha256(payload).hexdigest()
    headers = {"host": host, "x-amz-content-sha256": digest, "x-amz-date": timestamp}
    signed = ";".join(sorted(headers))
    canonical = "\n".join([
        method, path, "",
        "".join(name + ":" + headers[name] + "\n" for name in sorted(headers)),
        signed, digest,
    ])
    scope = date + "/homepool/s3/aws4_request"
    signing_key = sign(("AWS4" + request["secret"]).encode(), date)
    for value in ("homepool", "s3", "aws4_request"):
        signing_key = sign(signing_key, value)
    to_sign = "AWS4-HMAC-SHA256\n" + timestamp + "\n" + scope + "\n" + hashlib.sha256(canonical.encode()).hexdigest()
    signature = hmac.new(signing_key, to_sign.encode(), hashlib.sha256).hexdigest()
    headers["authorization"] = (
        "AWS4-HMAC-SHA256 Credential=" + request["key_id"] + "/" + scope
        + ", SignedHeaders=" + signed + ", Signature=" + signature
    )
    connection = http.client.HTTPConnection(request["host"], 3900, timeout=10)
    connection.request(method, path, body=payload, headers=headers)
    response = connection.getresponse()
    body = response.read()
    if action.startswith("deny-"):
        return response.status == 403 and b"AccessDenied" in body
    if action == "get":
        return response.status == 200 and body == b"homepool shared storage\n"
    return response.status == 200


try:
    success = run()
except Exception:
    success = False
sys.exit(0 if success else 1)
