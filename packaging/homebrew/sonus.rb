# Homebrew cask. Lives in a tap (github.com/kidchenko/homebrew-tap) as
# Casks/sonus.rb, then: brew install --cask kidchenko/tap/sonus
#
# The tap's bump workflow rewrites `version` and `sha256` from the latest release.
cask "sonus" do
  version "0.0.1"
  sha256 "REPLACE_WITH_SHA256"

  url "https://github.com/kidchenko/sonus/releases/download/v#{version}/Sonus-#{version}.dmg"
  name "Sonus"
  desc "Per-app volume mixer for the menu bar"
  homepage "https://github.com/kidchenko/sonus"

  livecheck do
    url :url
    strategy :github_latest
  end

  # Process taps, which per-app volume is built on, arrived in 14.2.
  depends_on macos: ">= :sonoma"

  app "Sonus.app"

  # Ad-hoc signed, not notarized: without this, Gatekeeper refuses to open it.
  postflight_steps do
    run "/usr/bin/xattr",
        args:           ["-dr", "com.apple.quarantine", "{{appdir}}/Sonus.app"],
        writable_paths: ["{{appdir}}/Sonus.app"]
  end

  uninstall launchctl: "com.kidchenko.sonus",
            quit:      "com.kidchenko.sonus"

  zap trash: [
    "~/Library/Application Support/Sonus",
    "~/Library/LaunchAgents/com.kidchenko.sonus.plist",
  ]
end
