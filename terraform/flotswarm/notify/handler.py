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


def _fmt_dur(dur):
    return f"{dur:.1f}s" if isinstance(dur, (int, float)) else "?"


def _render(msg):
    """Decode one message into presentation fields.

    Returns a dict with Discord fields (title/body/color) and email fields
    (email flag, subject, text). Email subjects are inbox-friendly and
    prefixed `flotswarm`; email bodies are plain text (no Discord markdown).
    """
    kind = msg.get("kind")
    if kind == "outcome":
        ok = bool(msg.get("ok"))
        action, host = msg.get("action", "?"), msg.get("host", "?")
        dur = _fmt_dur(msg.get("duration_s"))
        code = msg.get("exit_code")
        tail = msg.get("tail") or ""
        src = msg.get("source") or ""
        icon = "✅" if ok else "❌"
        result = "succeeded" if ok else f"failed (exit {code})"
        title = f"{icon} {action} @ {host}: {result}"
        subject = f"flotswarm {icon} {action} @ {host} — {'ok' if ok else 'FAILED'}"
        lines = [f"Action:   {action}", f"Host:     {host}",
                 f"Result:   {result}", f"Duration: {dur}"]
        if src:
            lines.append(f"Source:   {src}")
        text = "\n".join(lines)
        if tail:
            text += "\n\n--- output tail ---\n" + tail[:1800]
        dbody = f"{dur}\n```\n{tail[:1500]}\n```" if tail else dur
        return {"title": title, "body": dbody,
                "color": COLORS["ok"] if ok else COLORS["error"],
                "email": True, "subject": subject, "text": text}
    if kind == "dispatch":
        action, hook = msg.get("action", "?"), msg.get("hook", "?")
        targets = ", ".join(msg.get("targets") or []) or "?"
        src = msg.get("source") or ""
        outcome = msg.get("outcome", "")
        rejected = outcome.startswith(("rejected", "ignored"))
        title = f"📤 dispatch: {action} → {targets}"
        dbody = f"hook `{hook}` {outcome}" + (f"\nsource: {src}" if src else "")
        subject = f"flotswarm ⚠ dispatch {outcome}: {action}"
        lines = [f"Hook:     {hook}", f"Action:   {action}",
                 f"Targets:  {targets}", f"Outcome:  {outcome}"]
        if src:
            lines.append(f"Source:   {src}")
        return {"title": title, "body": dbody, "color": COLORS["dispatch"],
                "email": rejected, "subject": subject, "text": "\n".join(lines)}
    if "AlarmName" in msg:
        name = msg.get("AlarmName", "?")
        state = msg.get("NewStateValue", "?")
        reason = msg.get("NewStateReason", "")
        icon = "🚨" if state == "ALARM" else ("✅" if state == "OK" else "ℹ️")
        title = f"{icon} alarm {state}: {name}"
        subject = f"flotswarm {icon} {state}: {name}"
        text = f"Alarm:  {name}\nState:  {state}\n\n{reason}"
        return {"title": title, "body": reason, "color": COLORS["alarm"],
                "email": state == "ALARM", "subject": subject, "text": text}
    # Unknown shape — surface it rather than drop it.
    dump = json.dumps(msg, indent=2)
    return {"title": "flotswarm event", "body": dump[:1500],
            "color": COLORS["dispatch"], "email": False,
            "subject": "flotswarm event", "text": dump[:1800]}


def handler(event, _ctx):
    for rec in event.get("Records", []):
        raw = rec.get("Sns", {}).get("Message", "")
        try:
            msg = json.loads(raw)
        except (ValueError, TypeError):
            msg = {"kind": None, "raw": raw}
        r = _render(msg)
        try:
            _post_discord(r["title"], r["body"], r["color"])
        except Exception as e:  # noqa: BLE001 — never fail the whole batch on one channel
            print(f"discord post failed: {e}")
        if r["email"]:
            try:
                _send_email(r["subject"], r["text"])
            except Exception as e:  # noqa: BLE001
                print(f"ses send failed: {e}")
    return {"ok": True}
