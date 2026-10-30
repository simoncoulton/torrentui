class Torrentui < Formula
  desc "Terminal BitTorrent client with a background daemon, tags and post-download actions"
  homepage "https://github.com/simoncoulton/torrentui"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/simoncoulton/torrentui/releases/download/v0.1.0/torrentui-v0.1.0-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_intel do
      url "https://github.com/simoncoulton/torrentui/releases/download/v0.1.0/torrentui-v0.1.0-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/simoncoulton/torrentui/releases/download/v0.1.0/torrentui-v0.1.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_intel do
      url "https://github.com/simoncoulton/torrentui/releases/download/v0.1.0/torrentui-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "torrentui"
  end

  # brew services start torrentui keeps the daemon running at login
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
