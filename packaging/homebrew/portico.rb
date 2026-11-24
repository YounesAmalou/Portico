# The formula in the Homebrew tap. scripts/homebrew-formula.sh fills in the version and the
# checksums of the release's macOS tarballs, and the release workflow pushes the result to
# the tap; edit this template only when the packaging itself changes.
class Portico < Formula
  desc "Keyboard-first terminal UI and CLI for managing OpenSSH hosts"
  homepage "https://github.com/YounesAmalou/Portico"
  version "@VERSION@"
  license any_of: ["MIT", "Apache-2.0"]

  depends_on :macos

  on_arm do
    url "https://github.com/YounesAmalou/Portico/releases/download/v@VERSION@/portico-v@VERSION@-aarch64-apple-darwin.tar.gz"
    sha256 "@AARCH64_SHA256@"
  end
  on_intel do
    url "https://github.com/YounesAmalou/Portico/releases/download/v@VERSION@/portico-v@VERSION@-x86_64-apple-darwin.tar.gz"
    sha256 "@X86_64_SHA256@"
  end

  def install
    bin.install "portico"
  end

  test do
    assert_match "portico #{version}", shell_output("#{bin}/portico --version")
  end
end
