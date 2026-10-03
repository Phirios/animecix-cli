class Animecix < Formula
  desc "Watch and download Turkish-subtitled anime from Animecix"
  homepage "https://github.com/Phirios/animecix-cli"
  url "https://github.com/Phirios/animecix-cli.git", tag: "v0.1.0"
  version "0.1.0"
  license "MIT"
  head "https://github.com/Phirios/animecix-cli.git", branch: "main"

  depends_on "rust" => :build
  depends_on "ffmpeg"

  def install
    system "cargo", "install", *std_cargo_args
    generate_completions_from_executable(bin/"animecix", "--completions",
                                        shells: [:bash, :zsh, :fish])
  end

  def caveats
    <<~EOS
      Playback needs a player such as mpv, VLC, or IINA.
      For example: brew install mpv
                   animecix "one piece" --player mpv
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/animecix --version")
    assert_match "--download", shell_output("#{bin}/animecix --help")
    assert_match "animecix", shell_output("#{bin}/animecix --completions bash")
  end
end
