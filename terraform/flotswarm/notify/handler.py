"""flotswarm-notify — SNS-triggered relay to Discord + email (SES).

Subscribed to the flotswarm notify topic. Each SNS record carries either a
flotswarm NotifyEvent (`{"kind": "dispatch"|"outcome", ...}`, published by the
distributor and the agents) or a CloudWatch alarm message. This formats each and
delivers to Discord (all events) and email (only the ones worth an inbox hit:
failed outcomes, firing alarms, rejected hooks).

All config is env/SSM — no infra specifics live in this (public) repo:
  DISCORD_WEBHOOK_SSM  SSM param holding the Discord webhook URL (SecureString)
  EMAIL_FROM           verified SES sender (empty → email disabled)
  EMAIL_TO             comma-separated recipients
  AWS_REGION           (provided by Lambda)
"""
import json
import os
import urllib.request

import boto3

_ssm = boto3.client("ssm")
_ses = boto3.client("ses")
_webhook = None  # cached across warm invocations

COLORS = {"ok": 0x2ECC71, "error": 0xE74C3C, "dispatch": 0x3498DB, "alarm": 0xE67E22}


def _discord_webhook():
    global _webhook
    if _webhook is None:
        name = os.environ["DISCORD_WEBHOOK_SSM"]
        _webhook = _ssm.get_parameter(Name=name, WithDecryption=True)["Parameter"]["Value"]
    return _webhook


def _post_discord(title, body, color):
    payload = {
        "embeds": [{
            "title": title[:256],
            "description": body[:4000] if body else None,
            "color": color,
        }]
    }
    req = urllib.request.Request(
        _discord_webhook(),
        data=json.dumps(payload).encode(),
        # Discord 403s the default Python-urllib UA — set an explicit one.
        headers={"Content-Type": "application/json", "User-Agent": "flotswarm-notify/1.0"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        r.read()


def _send_email(subject, body):
    frm = os.environ.get("EMAIL_FROM", "")
    to = [a.strip() for a in os.environ.get("EMAIL_TO", "").split(",") if a.strip()]
    if not frm or not to:
        return
    _ses.send_email(
        Source=frm,
        Destination={"ToAddresses": to},
        Message={
            "Subject": {"Data": subject[:200]},
            "Body": {"Text": {"Data": body or subject}},
        },
    )


def _render(msg):
    """(discord_title, discord_body, color, email?) for one decoded message."""
    kind = msg.get("kind")
    if kind == "dispatch":
        action, hook = msg.get("action", "?"), msg.get("hook", "?")
        targets = ", ".join(msg.get("targets") or []) or "?"
        src = msg.get("source") or ""
        outcome = msg.get("outcome", "")
        rejected = outcome.startswith(("rejected", "ignored"))
        title = f"📤 dispatch: {action} → {targets}"
        body = f"hook `{hook}` {outcome}" + (f"\nsource: {src}" if src else "")
        return title, body, COLORS["dispatch"], rejected
    if kind == "outcome":
        ok = bool(msg.get("ok"))
        action, host = msg.get("action", "?"), msg.get("host", "?")
        dur = msg.get("duration_s")
        code = msg.get("exit_code")
        tail = msg.get("tail") or ""
        status = "✅ ok" if ok else f"❌ error (exit {code})"
        title = f"{status}: {action} @ {host}"
        meta = f"{dur:.1f}s" if isinstance(dur, (int, float)) else ""
        body = (f"{meta}\n```\n{tail[:1500]}\n```" if tail else meta)
        return title, body, COLORS["ok"] if ok else COLORS["error"], not ok
    if "AlarmName" in msg:
        name = msg.get("AlarmName", "?")
        state = msg.get("NewStateValue", "?")
        reason = msg.get("NewStateReason", "")
        title = f"🚨 alarm {state}: {name}"
        return title, reason, COLORS["alarm"], state == "ALARM"
    # Unknown shape — surface it rather than drop it.
    return "flotswarm event", json.dumps(msg)[:1500], COLORS["dispatch"], False


def handler(event, _ctx):
    for rec in event.get("Records", []):
        raw = rec.get("Sns", {}).get("Message", "")
        try:
            msg = json.loads(raw)
        except (ValueError, TypeError):
            msg = {"kind": None, "raw": raw}
        title, body, color, email = _render(msg)
        try:
            _post_discord(title, body, color)
        except Exception as e:  # noqa: BLE001 — never fail the whole batch on one channel
            print(f"discord post failed: {e}")
        if email:
            try:
                _send_email(title, (body or "").replace("```", ""))
            except Exception as e:  # noqa: BLE001
                print(f"ses send failed: {e}")
    return {"ok": True}
