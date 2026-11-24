# Loaded by yt-dlp as a plugin (locate.rs): prints which player JS variant this yt-dlp uses, then ends yt-dlp.
import json
import os

from yt_dlp.extractor.youtube import YoutubeIE as Y

print('YTSPRINT ' + json.dumps({'player_js': Y._PLAYER_JS_VARIANT_MAP[Y._DEFAULT_PLAYER_JS_VARIANT]}), flush=True)
os._exit(0)
