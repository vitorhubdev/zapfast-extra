# frozen_string_literal: true

# native-packages supplies an owned copy, already signed when Apple credentials
# are configured. Keep it unchanged and package only this input.
#
# The image is given an explicit size and filled through a mount instead of
# letting hdiutil size its own scratch image, and every step reports what it
# did: a runner that cannot write the image has to say why.
require "fileutils"
require "shellwords"
require "tmpdir"

payload, output = ARGV
abort "usage: dmg.rb PAYLOAD OUTPUT.dmg" unless payload && output
abort "output already exists: #{output}" if File.exist?(output)

def run(*command)
  puts "+ #{command.join(" ")}"
  system(*command)
end

# The payload in mebibytes, with headroom for the filesystem and the copy.
def image_size(staging)
  kilobytes = `du -sk #{Shellwords.escape(staging)} 2>/dev/null`.split.first.to_i
  abort "could not measure the payload" if kilobytes <= 0
  ((kilobytes / 1024.0) * 1.15).ceil + 64
end

def report(staging, output)
  destination = Shellwords.escape(File.dirname(File.expand_path(output)))
  puts "payload: #{`du -sh #{Shellwords.escape(staging)} 2>/dev/null`.strip}"
  puts "destination: #{`df -h #{destination} | tail -1`.strip}"
  puts "attached images: #{`hdiutil info | grep -c "^image-path"`.strip}"
end

Dir.mktmpdir("vespera-dmg-") do |directory|
  staging = File.join(directory, "stage")
  FileUtils.mkdir_p(staging)
  FileUtils.cp_r(File.join(payload, "."), staging, preserve: true)
  File.symlink("/Applications", File.join(staging, "Applications"))
  report(staging, output)
  raw = File.join(directory, "vespera.dmg")
  mount = File.join(directory, "mount")
  FileUtils.mkdir_p(mount)
  abort "DMG creation failed" unless run("hdiutil", "create", "-size", "#{image_size(staging)}m",
    "-fs", "HFS+", "-volname", "Vespera", raw)
  abort "DMG mount failed" unless run("hdiutil", "attach", raw, "-nobrowse", "-mountpoint", mount)
  copied = false
  begin
    copied = run("ditto", staging, mount)
  ensure
    run("hdiutil", "detach", mount)
  end
  abort "DMG copy failed" unless copied
  abort "DMG compression failed" unless run("hdiutil", "convert", raw, "-format", "UDZO",
    "-o", output)
  abort "DMG verification failed" unless run("hdiutil", "verify", output)
end

