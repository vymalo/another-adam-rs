class AdamAgent < Formula
  desc "Serve an agent folder over A2A: one static binary, SQLite on a laptop"
  homepage "https://github.com/vymalo/another-adam-rs"
  version "0.6.1"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/vymalo/another-adam-rs/releases/download/v0.6.1/adam-agent_0.6.1_darwin_arm64.tar.gz"
      sha256 "1111111111111111111111111111111111111111111111111111111111111111"
    end

    on_intel do
      url "https://github.com/vymalo/another-adam-rs/releases/download/v0.6.1/adam-agent_0.6.1_darwin_amd64.tar.gz"
      sha256 "2222222222222222222222222222222222222222222222222222222222222222"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/vymalo/another-adam-rs/releases/download/v0.6.1/adam-agent_0.6.1_linux_arm64.tar.gz"
      sha256 "3333333333333333333333333333333333333333333333333333333333333333"
    end

    on_intel do
      url "https://github.com/vymalo/another-adam-rs/releases/download/v0.6.1/adam-agent_0.6.1_linux_amd64.tar.gz"
      sha256 "4444444444444444444444444444444444444444444444444444444444444444"
    end
  end

  def install
    bin.install "adam-agent"
  end

  test do
    system bin/"adam-agent", "--version"
  end
end
