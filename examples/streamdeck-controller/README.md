# Ferrocast Stream Deck Style Controller (Python)

Zero-dependency Python 3 controller that acts like a hardware keypad / Stream Deck deck against Ferrocast using a scoped local API key (`Bearer fc_key_...`).

## Required key scopes

Create a local API key in Ferrocast (**Settings > Advanced > Local API keys**) with:

- `studio_control` (switch OBS scenes, mute/unmute inputs, drop stream markers)
- `alerts` (pause, resume, skip alerts, or fire test alerts)
- `read_only` (list scenes, inputs, queue, status)

## Where it connects

`FERROCAST_URL` defaults to `http://127.0.0.1:47820`. The key travels in every request, so plain `http://` is accepted for this machine only (127.x, `::1`, `localhost`); any other address must be `https://`. Redirects are never followed, so the key can't be sent on to another address (a redirect is reported as an error).

## Usage

```sh
export FERROCAST_API_KEY="fc_key_..."

python3 controller.py status
python3 controller.py scenes
python3 controller.py scene Main
python3 controller.py mute Mic --off
python3 controller.py marker "Clutch round"
python3 controller.py alerts skip
python3 controller.py alerts test follow
python3 controller.py deck
```

## Test

```sh
python3 -m unittest test_controller.py
```
