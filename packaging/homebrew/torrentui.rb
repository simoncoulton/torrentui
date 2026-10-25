class Torrentui < Formula
  desc "Terminal BitTorrent client with a background daemon, tags and post-download actions"
  homepage "https://github.com/simoncoulton/torrentui"
  url "https://github.com/simoncoulton/torrentui/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "REPLACE_WITH_SHA256_OF_THE_TARBALL"
  license "MIT"
  head "https://github.com/simoncoulton/torrentui.git", branch: "master"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  # `brew services start torrentui` keeps the daemon running at login
  # (launchd on macOS, systemd on Linux).
  service do
    run [opt_bin/"torrentui", "daemon", "run"]
    keep_alive successful_exit: false
    log_path var/"log/torrentui.log"
    error_log_path var/"log/torrentui.log"
  end

  def caveats
    <<~EOS
      torrentui runs a background daemon that starts on demand. To keep it
      running at login:
        brew services start torrentui

      To open magnet links from your browser:
        torrentui register
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/torrentui --version")
    assert_match "not running", shell_output("#{bin}/torrentui daemon status", 3)
  end
end
