# What's new in Ferrocast

Each release's section is what the app shows under "What's new" before you update, and
the text of its GitHub release. Write it for streamers: what changed for them, in plain
words. The Release workflow reads the section whose heading is the version being released
(`packaging/release-notes.sh`).

## 2.1.2

- Updates download in seconds. Ferrocast now fetches updates from a fast mirror, so a
  46 MB update no longer takes minutes in India.
- The update window no longer scrolls the page behind it.
- "What's new" reads properly: sentences no longer break into separate lines, and bold
  text shows as bold.
- The website's Download buttons use the same fast mirror.

## 2.1.1

**The Studio: stream without OBS.** Ferrocast now has OBS's own engine inside it. In Studio ›
Go live, Ferrocast shows your scenes, sources, sound and the live picture in one window, and
goes live to Twitch, YouTube and Kick straight from there.

- Bring your setup: import your OBS scene collections, or Streamlabs Desktop's, in one click.
  Missing files are found for you.
- Everything OBS does: scenes and sources, groups, filters, transitions and stingers, Studio
  Mode with a T-bar, projectors and a multiview, scene collections and profiles, every hotkey,
  undo for everything with a history list.
- Arrange it in the preview: drag sources, resize them by their handles, see every source's
  outline in its own colour, zoom out to see what's off the screen. Drag a source up or down
  in the list to put it on top.
- Recording as OBS has it: MP4 and MOV that survive a crash, splitting, chapters, any
  encoder, replay buffer and Save replay.
- Send straight by SRT, RIST or WebRTC too, next to your platforms.
- A mixer with true-peak meters, monitoring, advanced audio, and low latency sound.
- Works with OBS 32 and OBS 33.
- Set up for me picks good settings for your computer and your upload. The log opens in the
  app with your keys taken out.

**Community** is cleaner: Poll, Giveaway and Goals side by side, your top viewers across the
page, and long lists and settings open in a side panel with search.

**Settings** use the whole window, and the page you're on is plain to see in orange.

## 2.0.0

**Go live everywhere.** Stream once from OBS, and Ferrocast sends it to Twitch, YouTube and Kick
at the same time, plus any server of your own.

- No stream keys to paste: Ferrocast gets each one itself. One click points OBS at Ferrocast,
  one click puts it back. Reconnect Twitch and Kick once so Ferrocast may read the key.
- Test your upload first: Ferrocast says in plain words how many platforms fit.
- While live, each platform has its own health line. One that drops reconnects by itself and
  never stops the others. If your upload can't carry them all, the bottom one pauses for a
  while so the others stay smooth, then comes back.
- Each platform's rules are checked against what OBS really sends, with the number to set.
  Kick takes up to 8,000 kbps: above that it keeps dropping.
- Give Twitch, YouTube or Kick its own title, category or tags.
- Vertical streams: YouTube Shorts live (or your own server) next to your normal stream, from
  OBS's vertical canvas or cut from your stream on your graphics card.
- A note if Aitum Multistream also sends to a platform, so nobody gets your stream twice.

**Themes in OBS.** Put a theme into OBS in one click: a scene per screen with your own game
and camera scene under it, Start show set up, old StreamElements or Streamlabs widgets hidden.
Make it yours changes the colour, font, your name and logo everywhere. Take it out removes only
what Ferrocast added.

**Eleven new themes** with a live world behind every break screen: Hex Pulse, Night Drive,
Wasteland, Circuit, Neon Tunnel, Night Dunes, Lava Forge, Arena Lights, Code Rain City, Orbit
and Deep Sonar. Each has eight screens, with Getting ready, Paused and Offline added.

**Music and sound on your screens.**

- The wave player or a heartbeat line with the song, shown only while music plays, and
  backgrounds that move with the beat. Settings, Music picks where the beat and the song come
  from (Spotify or Music on a Mac, your player on Windows, or a text file).
- A sound element for any screen: a whoosh or riser when it comes on, rain or a soft pad behind
  it, ticks in a timer's last ten seconds and a hit at zero. Real recordings, free to use.
- Timers have six looks: digital, thin, ring, bar, flip cards and words.
- Start show's countdown: choose what happens at zero (switch scene, unmute, start streaming or
  recording, a chat message, a marker), each on or off.

**Also:** Simple and Advanced in Studio, much less text (explanations behind an (i)), and a
log file in Ferrocast's data folder for looking into problems.

## 2.0.0-beta.13

- The sound element uses real recordings: a whoosh, a riser, a cinematic hit, a clock tick, a
  chime and a glitch, plus rain, cold wind, a soft pad and a city at night as loops.

## 2.0.0-beta.12

- Starting soon stays a plain screen when no music plays: the music panel comes and goes with
  the music.
- Long titles on the new themes get smaller instead of wrapping onto the line below.
- Fixes found testing beta.11 in OBS.

## 2.0.0-beta.11

- Eleven new themes with a live world behind every break screen: Hex Pulse, Night Drive,
  Wasteland, Circuit, Neon Tunnel, Night Dunes, Lava Forge, Arena Lights, Code Rain City, Orbit
  and Deep Sonar. Each has eight screens, with Getting ready, Paused and Offline added.
- Music on your screens: the wave player or a heartbeat line with the song, shown only while
  music plays, and backgrounds that move with the beat. Pick where the beat and the song come
  from in Settings, Music (Spotify or Music on a Mac, your player on Windows, or a text file).
- Sound on any screen: a whoosh or riser when it comes on, rain or a synth pad behind it, ticks
  in a timer's last ten seconds and a hit at zero. New overlays in OBS send their sound
  through OBS, so viewers hear it.
- Timers have six looks: digital, thin, ring, bar, flip cards and words.
- Start show's countdown: choose what happens at zero (switch scene, unmute, start streaming or
  recording, a chat message, a marker), each on or off. Timers on screen follow it.

## 2.0.0-beta.10

- A theme in OBS keeps every choice: switching theme or Make it yours keeps where each screen
  goes, what's under it, its Start show job and the mic. The dialog opens on last time's
  choices, and unticking a screen later takes it out of OBS.
- Mic on break screens uses your game scene's mic, never OBS's global Mic/Aux (which viewers
  would hear twice).
- Your theme in OBS updates by itself when something changes it from another tab or a phone.
- Ferrocast keeps a log file (in its data folder, under logs) to look into problems.

## 2.0.0-beta.9

- Mic on break screens: in Advanced, Starting, BRB and Ending can get your mic. Never twice,
  and turning it off takes out only the mics Ferrocast added.
- Make it yours changes the whole colour family, so title gradients follow your colour too.
- Live safety: putting a theme in while you're live never switches what viewers see, and
  Take it out waits while you're live on one of the theme's scenes.
- Quick changes in Make it yours are never lost.

## 2.0.0-beta.8

- Put a theme into OBS in one click. Use on a theme can set it up in OBS too: a scene per
  screen with your own game and camera scene under it, Start show's scenes set, and old
  StreamElements or Streamlabs widgets hidden. Tick the screens you want, or switch to Advanced
  to pick every scene. Undo takes out only what Ferrocast added.
- Make it yours: change a theme's colour, font, your name and logo on every screen at once.
  Screens in OBS follow your theme until you edit one by hand.
- Your theme in OBS, on Studio's OBS tab: make it yours, switch theme in the same scenes, or
  take it out.
- Much less text: explanations are behind an (i) you hover or tap.
- The vertical preview shows exactly the part of the box phones get.
- Stream info's Send to is three switches in one row.

## 2.0.0-beta.7

- Cut the vertical picture from your stream: no vertical canvas needed. Mark your camera and
  game on OBS's picture, pick a layout (camera on top, small camera, half and half, game only)
  and Ferrocast makes the upright stream on your graphics card. The first time, it fetches
  ffmpeg by itself (about 25 MB, checked before it's used).
- Simple and Advanced: Studio has a switch at the top. Advanced shows upload protection's rule
  with its numbers, a server address and retry timing per platform, the local server's port and
  a new stream key, and the vertical layout in exact pixels.
- Pointing OBS at Ferrocast also fixes an old stream key when OBS already points here.

## 2.0.0-beta.6

Everything in beta.5 (its build was stopped), plus two fixes from a real test:

- Aitum Multistream 1.0.8 is noticed too (it doesn't answer OBS's remote control).
- When upload protection pauses a platform, it names the one it keeps smooth correctly.

## 2.0.0-beta.5

- Vertical streams: Studio has a Vertical tab. Point Aitum Vertical (OBS's vertical canvas) at
  Ferrocast in one click, and it goes live on YouTube Shorts or a server of your own, next to
  your normal stream. Ferrocast makes the YouTube stream for you; nothing to paste.
- Stream info per platform: give Twitch, YouTube or Kick its own title, category or tags. The
  Everywhere tab shows what each platform changed.
- If Aitum Multistream also sends to a platform Go live sends to, Ferrocast says so before you
  go live, and while live if a platform gets your stream twice.

## 2.0.0-beta.4

- Upload protection for Go live everywhere: if your upload can't carry every platform, the
  bottom one in your list pauses for a while so the others stay smooth, then comes back by
  itself. Its health line says why, with Resume now. Turn it off under Your platforms.
- A platform that keeps dropping a live stream now starts its retries from the shortest wait
  each time, instead of waiting longer and longer.

## 2.0.0-beta.3

- Go live everywhere (beta): stream once from OBS, and Ferrocast sends it on to Twitch,
  YouTube and Kick at the same time, plus any server of your own. Find it first in Studio.
- Ferrocast gets each platform's stream key by itself, so there's nothing to paste. Reconnect
  Twitch and Kick once so Ferrocast may read the key.
- One click points OBS at Ferrocast, and one click puts OBS back the way it was.
- Test your upload before you go live: Ferrocast says in plain words how many platforms fit.
- While live, each platform has its own health line. One that drops reconnects by itself and
  never stops the others.
- Start streaming in Studio no longer says "OBS stopped streaming" right after starting.

## 1.4.2

- Go live and end your stream from Ferrocast: Studio shows whether OBS is streaming, with
  Start streaming and Stop streaming (it asks first). The go-live checklist can also press
  them for you, and stop a little after your ending scene so viewers see it.
- Stream info can set your YouTube description and thumbnail too. Twitch and Kick don't
  allow either, and Ferrocast says so next to each one.
- Pick Light, Dark or Auto (follows your computer) right in the sidebar.
- The tip page settings show your banner and avatar the way tippers see them.
- The ... menu on your designs no longer gets cut off.
- The alert designer plays sound while you design (it used to start muted).
- Ferrocast's emails (sign-in, weekly report) have a cleaner new look.

## 1.4.1

- Connecting YouTube or Kick opens the sign-in page in your browser by itself. If your
  browser doesn't open, Ferrocast says so, and you can copy the link and paste it in.

## 1.4.0

- Cloud mode, free with an account: run alerts, overlays and the chat bot from Ferrocast's
  servers, set up from the web dashboard on any browser. Your PC can stay off.
- On the PC, Settings › Cloud mode can sync your overlays, alerts and designs with your
  account and use the cloud overlay links in OBS. Both are off until you turn them on.
- Insights (was History): charts for each stream, the best times to stream, top supporters
  and a CSV export. 30 days with a free account, a year with Pro.
- New in Pro: smart moderation that understands Hinglish (every action can be undone),
  smart replies from facts you approve, and a written recap after each stream.
- New in Pro: YouTube chat and the bot from the cloud, without your PC.

## 1.3.1

- Updates arrive on more networks: if one of GitHub's download servers doesn't answer,
  Ferrocast tries another instead of giving up.
- On a Mac, clicking Ferrocast in the Dock opens the window again after you've closed it.
- See which version you run: under the Settings menu, and at the top of the tray menu.

## 1.3.0

- Ferrocast accounts, free and optional. Sign in with Google or an email link from the bottom
  of the sidebar. The app still works without one.
- With an account, the web dashboard works from your phone or any browser, free.
- Pro now belongs to your account: sign in on another PC and it's there (up to 3 PCs).
- Get Pro opens a plans page inside the app; you pay in your browser and Pro turns on here.
- A new Account page: your plan and renewal, billing, signed-in devices (sign out any of
  them) and "Have a code?".

## 1.2.0

- Have a Pro code? Enter it in Settings › Pro. A free-Pro code turns Pro on right away; a
  discount code lowers the price at checkout, and you keep that price when it renews.
- When Pro is on sale, Settings › Pro shows the sale price with the old price crossed out.

## 1.1.0

- Updates you can see: when a new version is out, the sidebar says so. Read what's new,
  press Update now, watch the download, then restart when you're ready. Ferrocast never
  restarts on its own, and never while you're live.
- Settings › Updates has Check for updates, shows when it last looked, and says what went
  wrong (offline, a failed signature check) and what to do.
- The motion designer: 11 entrances, 9 text effects, 9 exits, idle loops, particles,
  punches and recipes for every alert look. Bigger tips can hit harder.
- New built-in sounds (Crystal and Felt sets) in a picker that plays as you browse.
  New alerts start with a suggested sound for their event.
- Buy Pro in three clear steps in Settings › Pro, and Pro now looks and feels like Pro.
- Fixed in 1.0.1: links to the browser, the Mac installer's background, alert previews
  and uploaded pictures in the designer.

## 1.0.1

- Links to the web (Twitch activation, checkout, tip and goal pages) open in your browser
  from the Mac app.
- The Mac installer window shows its background and icon places again.

## 1.0.0

- The first stable release: alerts, overlays, merged chat, the chatbot and moderation for
  Twitch, YouTube and Kick, all on your own computer.
