# Virtual camera on Windows

Owner, 12 Nov 2026: "build it… make it super good". The old note said Windows needs a paid,
signed camera driver. It doesn't: neither of Windows' two ways of adding a camera is a driver.

## The two ways, and which one shows where

| | DirectShow source filter | Media Foundation virtual camera |
| --- | --- | --- |
| What it is | A COM DLL every DirectShow app loads into itself (OBS's way) | A COM media source the Windows Camera Frame Server loads, made a camera with `MFCreateVirtualCamera` |
| Windows | 10 and 11 | 11 (build 22000) and later |
| Seen by | Zoom, Discord, OBS, Chrome/Edge, Skype, older Teams… | Every camera app: the above, plus Windows Camera, new Teams, apps on WinRT `MediaCapture` |
| Needs admin | No: registered for the user (HKCU) | Once: the frame server runs as LocalService and reads HKLM and a folder it can read |
| Signing | None | None |

Ferrocast installs per user, with no admin. So:

1. **Always:** the DirectShow camera, registered for the user the first time the camera starts.
   No prompt. Both the 64-bit and the 32-bit DLL, so 32-bit apps see it too.
2. **On Windows 11, when the user asks:** "Show in every app" runs one elevated step (one UAC
   prompt). It copies the camera DLL to `%ProgramFiles%\Ferrocast Camera` and registers its
   media source in HKLM. From then on, starting the camera also creates the Media Foundation
   camera for this session, and the DirectShow registration is removed. Windows shows the
   Media Foundation camera to DirectShow apps as well, so there's never two "Ferrocast Camera"s.
   Windows may list it with "Windows Virtual Camera" added to its name: look for either.

## How frames travel

The app draws the camera's picture once per frame in the engine, as **NV12 at the camera's size**
(the canvas fitted inside 1920×1080, even sides). It shares the drawing with a recording of the
same size, and it's done on the GPU. The picture goes into shared memory (`ferrocast-camshare`):

- A 4 KiB header, then 3 frame slots. Each slot has a seqlock (odd while written), so a reader
  never shows a torn frame and the writer never waits.
- A heartbeat (the writer's clock). When it's older than a second, or the writer says it
  stopped, readers show a placeholder instead of a frozen picture.
- The size can change while running (the canvas changed). Readers scale to what their app
  agreed to.
- Version and magic fields: a reader built for another layout shows the placeholder rather
  than garbage.

Names and permissions:
- DirectShow readers run in the user's own apps: `Local\FerrocastCamera`, made by the app.
- The Media Foundation reader runs in the frame server (session 0, LocalService), which can't
  see the user's `Local\` objects. The app can't make `Global\` ones (no SeCreateGlobalPrivilege),
  so the media source makes `Global\FerrocastCameraMF`, readable and writable by interactive
  users. The app opens it while the camera is on and writes there too.

## Formats

- DirectShow: NV12, YUY2 and I420 (some apps want one or the other). Sizes: the writer's own
  size, 1920×1080, 1280×720 and 640×360, at the writer's rate and 30 fps. The filter scales
  when the app picks another size.
- Media Foundation: NV12 at the writer's size, or 1920×1080 when no writer is there yet. The
  frame server converts for apps.

## Edge cases

- Camera on, Ferrocast closed or crashed: the placeholder within a second, never a frozen frame.
- Ferrocast updated while an app has the camera open: the DLL in use stays loaded; the new one
  is used next time. The layout version guards against a mismatch.
- The canvas size changes: readers scale, so apps never need to reconnect.
- Two Ferrocast windows: the app is single-instance, so one writer.
- An app opens the camera before Ferrocast starts it: the placeholder, then the picture as soon
  as it starts.
- Several apps at once: each DirectShow app reads the shared memory on its own. Media
  Foundation's frame server shares one source among all apps.
- One source alone (`source`): the same path, that source's picture.
- Turning the camera off removes the Media Foundation camera (session lifetime: Windows removes
  it anyway when Ferrocast quits). The DirectShow camera stays listed and shows the placeholder,
  as OBS's does.
- Updates: the DLLs are named with Ferrocast's version (`ferrocast_camera-3.3.0.dll`,
  `ferrocast_camera32-…`). An app holding the camera open keeps the old file locked, so an
  update could never overwrite it: the new one goes beside it, the registration moves to it the
  next time the camera starts, and old ones are deleted once no app holds them.
- Uninstall: the NSIS uninstaller removes the DirectShow registration (both views) and old
  DLLs. The camera for every app is only shown while Ferrocast makes it, so nothing stays
  visible; "Camera in every app" off (UAC) removes its copy and class too.
- The Microsoft Store version can't: an MSIX app's registry writes go to its own private hive,
  so other apps would never see the camera, and it can't run the elevated step. It says so and
  points to the installer. (A later fix: the restricted capability `unvirtualizedResources`.)
- Windows "N" editions (no Media Foundation): the DLL looks Media Foundation up at run time, so
  it still loads there and the DirectShow camera works.
- Several people signed in to one PC (security review, 12 Nov): the frame server's region
  (`Global\FerrocastCameraMF`) is readable and writable by every interactive account, so while
  "Camera in every app" is on, another signed-in user could see or replace the camera's
  picture. Ferrocast wipes the region when the camera stops. To do, on a real PC: bind it to
  one user (find out what the frame server knows of the user it serves, then name the region
  by that user's SID and grant only that SID). DirectShow's region is the user's own.
- A canvas faster than 30 fps: the camera sends every second (or third) frame; video calls
  take 30.

## Tests

- `ferrocast-camshare`: layout, seqlock under a writer and readers racing (no torn frames), the
  heartbeat and placeholder, scaling and format conversion. Runs on every OS.
- `ferrocore`: frame taps get NV12 at their size and rate, and share drawing with outputs.
- `ferrocast-wincam` on the Windows runner:
  - Register per user, then FFmpeg lists "Ferrocast Camera" (`-list_devices`), and
    `-f dshow -i video="Ferrocast Camera"` grabs frames that match what was written.
  - The media source made in-process: stream descriptor, NV12 type, Start, RequestSample gives
    the written frame.
  - The DLL's exports, registration and unregistration.
