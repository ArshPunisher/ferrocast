# Google verification for YouTube (what the owner submits)

Ferrocast's YouTube features use the sensitive scope `youtube.force-ssl`. Until Google
verifies the app, its consent screen is in **Testing**: only the test users you list can sign
in (at most **100** Google accounts), they see an "unverified app" warning, and their sign-ins
**expire after 7 days** (the cloud bot then shows "Google signed the cloud bot out of
YouTube"). Verification lifts all three. This file is what to submit, in order.

`youtube.force-ssl` is *sensitive*, not *restricted*: no paid security assessment (CASA) is
needed. Expect a few business days for the brand check and a few weeks for the scope review,
with emails from Google asking for changes; answer them from the same account.

Separately, the **YouTube API Services audit and quota extension** form raises the 10,000
units a day (docs/API.md §9.7 quota and fair share). Ask for it once real streamers use the
cloud bot; Google reviews how the quota is used against the YouTube API Services Terms.

## 1. Which Google clients need it

| Client (project) | Type | Scopes | Needs |
|---|---|---|---|
| **Ferrocast Cloud** (`YOUTUBE_CLOUD_CLIENT_ID`) | Web application | `https://www.googleapis.com/auth/youtube.force-ssl` | **Sensitive scope verification** (this file) |
| Ferrocast desktop app (`FERROCAST_YOUTUBE_CLIENT_ID`, the releases' public id) | Desktop | `youtube.force-ssl`, `youtube.readonly` | The same review if its consent screen is in another project; in the same project one submission covers both |
| Ferrocast accounts (web) (`RELAY_GOOGLE_CLIENT_ID`) | Web application | `openid email profile` | Brand verification only (non-sensitive scopes) |

If Ferrocast Cloud is its own project (recommended in deploy/README.md, it gets its own 10,000
units), submit that project; if the desktop app's client lives in a second project, submit
that one too with the same texts, plus a line for `youtube.readonly`, which the desktop app
asks for as well (`crates/youtube/src/oauth.rs`).

## 2. Before you press Submit

1. **Domain.** In [Google Search Console](https://search.google.com/search-console) verify
   `clearcraftlabs.com` (a DNS TXT record at Cloudflare) with the same Google account that owns
   the Cloud project. The home page and the privacy page must be on that verified domain.
2. **Branding** (Google Auth Platform → Branding):
   - App name: `Ferrocast` (the name on the consent screen must match the home page; avoid
     "YouTube" or "Google" in it).
   - Logo: the app icon, 120 × 120 PNG, from `ferrocast-web/brand` (no Google or YouTube marks).
   - User support email and developer contact: the owner's support address.
   - Home page: `https://ferrocast.clearcraftlabs.com`
   - Privacy policy: `https://ferrocast.clearcraftlabs.com/privacy`
   - Terms of service: `https://ferrocast.clearcraftlabs.com/terms`
   - Authorised domains: `clearcraftlabs.com`
3. **Audience:** External, Publishing status **In production** (press *Publish app*; while in
   Testing the 100-user cap stays).
4. **Data access:** exactly `.../auth/youtube.force-ssl` for Ferrocast Cloud (and
   `youtube.readonly` only on the desktop client). Nothing else: every extra scope has to be
   justified too.
5. **Redirect URIs** on the Ferrocast Cloud client: only
   `https://ferrocast-relay.clearcraftlabs.com/v1/bot/youtube/callback`. The web dashboard's
   connect (no PC) uses the same callback, so nothing new is registered.
6. The privacy page has every item in §5 below (it does from 1.4; check it is deployed).

## 3. Scope justification (paste into "How will the scopes be used?")

> Ferrocast is a free, open-source companion app for live streamers (alerts, overlays and a
> chat bot for Twitch, YouTube and Kick). With Ferrocast Pro, a streamer can keep their chat
> bot running on our server when their own computer is off, or without a computer at all.
>
> We request https://www.googleapis.com/auth/youtube.force-ssl, which YouTube requires for
> the live chat endpoints we use, for the streamer's own channel only:
> 1. Reading their live chat while they are live (liveChatMessages.streamList), so the bot
>    can answer the commands and questions the streamer set up.
> 2. Posting the bot's replies and timed messages into that live chat
>    (liveChatMessages.insert), as the streamer's channel, as configured by the streamer.
> 3. Deleting a message or timing out or banning a viewer in that chat
>    (liveChatMessages.delete, liveChatBans.insert), only when the streamer or one of their
>    moderators presses the button, or when the streamer has turned on an automatic timeout
>    for clear abuse (always shown to them with an undo).
> 4. Finding their current live broadcast (liveBroadcasts.list) and their channel id and name
>    (channels.list?mine=true), to know when they are live and which channel they connected.
>
> youtube.readonly does not allow reading, posting to or moderating live chat, so a narrower
> scope cannot do this. We never read or change videos, playlists, comments, subscriptions or
> anything outside the live chat. Chat messages are processed in memory and never stored. The
> refresh token is encrypted on our server and deleted and revoked when the streamer
> disconnects, or 30 days after their subscription ends. We do not sell, share or use YouTube
> data for advertising, and our use complies with the Google API Services User Data Policy,
> including the Limited Use requirements.

## 4. The demo video (unlisted YouTube link, 2 to 4 minutes, English, no music needed)

Record in one take on a test channel with a live stream running (a private or unlisted
broadcast is fine). Speak or caption each step. Google checks that the consent screen shown is
the one being reviewed, so the **OAuth client ID must be visible in the browser's address
bar** at step 3.

1. **The app and who it is for** (15 s): `https://ferrocast.clearcraftlabs.com`, then sign in
   to the web dashboard (`https://ferrocast.clearcraftlabs.com/app/`) with a Ferrocast account
   that has Pro. (The video sent on 22 Oct shows the older address,
   `remote.clearcraftlabs.com/app/`; keep that host until the review ends, and give Google the
   new address if they ask.)
2. **Where the streamer starts it** (15 s): Account page → "YouTube from the cloud" →
   *Connect YouTube*.
3. **The consent screen** (30 s): Google's page opens. Zoom into the address bar to show
   `client_id=<Ferrocast Cloud's id>` and `scope=…youtube.force-ssl`. Show the app name and
   logo, pick the channel's Google account, show the permission text, press *Allow*.
4. **Back in Ferrocast** (15 s): the Account page shows the channel's name and "Connected".
   Switch the cloud bot on.
5. **Each use of the scope, on the live stream** (60 to 90 s), with the YouTube live chat open
   next to it:
   - Reading: type `!discord` in the live chat from a second (viewer) account; the bot's
     answer appears (reading + posting).
   - A timed message appearing.
   - Moderation: from the Moderation page delete a viewer's message, then time out the viewer;
     show it happen in YouTube's chat. Then press *Undo* and show the timeout lifted.
6. **Off and revoke** (30 s): press *Disconnect* (the token is revoked at Google), then open
   `https://myaccount.google.com/permissions` and show Ferrocast is gone. Mention that chat is
   never stored.
7. **The privacy policy** (15 s): scroll to the YouTube section on
   `https://ferrocast.clearcraftlabs.com/privacy`.

## 5. What the privacy page must say (checked by the reviewer)

On `https://ferrocast.clearcraftlabs.com/privacy` (apps/site/src/pages/privacy.astro):

- [ ] The **YouTube API Services** are used, with links to the
  [YouTube Terms of Service](https://www.youtube.com/t/terms) and the
  [Google Privacy Policy](https://policies.google.com/privacy).
- [ ] **What YouTube data** Ferrocast accesses: the channel's id and name, its live
  broadcast's chat id, live chat messages while live, and the moderation actions the streamer
  chooses.
- [ ] **How it's used**: only to run the streamer's bot, chat and moderation in their own
  live chat. Not for advertising, not sold, not shared except as needed to run the feature.
- [ ] **What is stored and for how long**: chat messages are never stored (memory only);
  the channel id and name and an encrypted refresh token are kept until the streamer
  disconnects, or 30 days after Pro ends; quota counters are kept about two months.
- [ ] **How to delete or revoke**: Disconnect in Ferrocast (revokes the token at Google), the
  [Google security settings page](https://security.google.com/settings/security/permissions),
  and the support email for deleting everything.
- [ ] The **Limited Use** sentence: "Ferrocast's use and transfer of information received from
  Google APIs adheres to the Google API Services User Data Policy, including the Limited Use
  requirements."
- [ ] If smart features (Pro) are on: which processor reads chat text for them (Cloudflare
  Workers AI, or Google Gemini if that key is used) and that nothing is stored.

## 6. While you wait

- Keep the consent screen in Testing and add each beta streamer's Google account under
  Audience → Test users (at most 100). Tell them their YouTube sign-in lasts 7 days.
- Once verified, nothing changes in the code or the relay: sign-ins stop expiring and the
  warning disappears.
