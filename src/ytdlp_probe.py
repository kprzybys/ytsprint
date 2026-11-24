# Loaded by yt-dlp as a plugin (locate.rs): prints the settings ytsprint shares with yt-dlp, then ends yt-dlp.
import json
import os

from yt_dlp.extractor.youtube import YoutubeIE as Y
from yt_dlp.extractor.youtube._base import INNERTUBE_CLIENTS

print('YTSPRINT ' + json.dumps({
    'player_js': Y._PLAYER_JS_VARIANT_MAP[Y._DEFAULT_PLAYER_JS_VARIANT],
    'clients': {
        name: {
            'id': INNERTUBE_CLIENTS[name]['INNERTUBE_CONTEXT_CLIENT_NAME'],
            'client': INNERTUBE_CLIENTS[name]['INNERTUBE_CONTEXT']['client'],
        } for name in ('web', 'web_creator', 'tv_samsung')
    },
}), flush=True)
os._exit(0)
