# The release workflow renders this template into the tap on every tag:
# it fills the version and the four checksums from the build's own
# archives, then pushes the result to cjohnhanson/homebrew-tap. Edit
# the formula here, never in the tap.
class Kersh < Formula
  desc "Declarative agent runner: a markdown file names a model, a skill, and a profile"
  homepage "https://github.com/cjohnhanson/kersh"
  license "MIT"
  version "0.0.0"

  on_macos do
    on_arm do
      url "https://github.com/cjohnhanson/kersh/releases/download/v#{version}/kersh-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "PLACEHOLDER_DARWIN_ARM64"
    end
    on_intel do
      url "https://github.com/cjohnhanson/kersh/releases/download/v#{version}/kersh-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "PLACEHOLDER_DARWIN_X64"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/cjohnhanson/kersh/releases/download/v#{version}/kersh-v#{version}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "PLACEHOLDER_LINUX_ARM64"
    end
    on_intel do
      url "https://github.com/cjohnhanson/kersh/releases/download/v#{version}/kersh-v#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "PLACEHOLDER_LINUX_X64"
    end
  end

  def install
    bin.install "kersh"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/kersh --version")
  end
end
