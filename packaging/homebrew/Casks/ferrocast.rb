# Homebrew cask for Ferrocast (tap: ArshPunisher/homebrew-ferrocast).
#   brew install --cask arshpunisher/ferrocast/ferrocast
# Every release attaches this file with the real version and checksum (release.yml runs
# update-cask.sh); copy that one into the tap. The values below are placeholders.
cask "ferrocast" do
  version "0.1.0"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"

  url "https://github.com/ArshPunisher/ferrocast/releases/download/v#{version}/Ferrocast_#{version}_universal.dmg",
      verified: "github.com/ArshPunisher/ferrocast/"
  name "Ferrocast"
  desc "Alerts, overlays, chat and moderation for Twitch, YouTube and Kick"
  homepage "https://ferrocast.clearcraftlabs.com/"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: ">= :ventura"

  app "Ferrocast.app"

  # Ferrocast is open source and not notarised (no paid Apple developer account), so macOS
  # would refuse to open it the first time. Installing through this tap is the user's
  # explicit choice, so the download quarantine flag is cleared.
  postflight do
    system_command "/usr/bin/xattr",
                   args: ["-dr", "com.apple.quarantine", "#{appdir}/Ferrocast.app"]
  end

  zap trash: [
    "~/Library/Application Support/Ferrocast",
    "~/Library/Application Support/com.clearcraftlabs.ferrocast",
    "~/Library/Caches/com.clearcraftlabs.ferrocast",
    "~/Library/LaunchAgents/Ferrocast.plist",
    "~/Library/Preferences/com.clearcraftlabs.ferrocast.plist",
    "~/Library/WebKit/com.clearcraftlabs.ferrocast",
  ]
end
