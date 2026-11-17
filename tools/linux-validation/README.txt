Inkbridge Linux validation kit
==============================

Checks the Linux host on a real desktop: hardware video encoding, Wayland
screen sharing, the pen as a mouse, and the pen with pressure.

1. Boot the PC from the Ubuntu USB stick and choose "Try Ubuntu".
2. Open the Files app, open the USB stick, open this "inkbridge" folder.
   (It may also be at /cdrom/inkbridge.)
3. Right-click inside the folder → "Open in Terminal", then type:

       bash run.sh

4. Answer the questions. When the desktop asks to share the screen or allow
   remote control, choose the monitor and allow it.
5. Have the tablet and its USB cable ready for steps 2 and 3.

Everything is saved to a results-<date> folder here (or in your home folder
if the stick can't be written; the script tells you where). Nothing is sent
anywhere. A live session forgets all changes when you shut down.
