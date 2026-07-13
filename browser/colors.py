"""CSS color -> (r, g, b) for the native rasterizer."""

NAMED = {
    "black": (0, 0, 0), "white": (255, 255, 255), "red": (255, 0, 0),
    "green": (0, 128, 0), "blue": (0, 0, 255), "yellow": (255, 255, 0),
    "cyan": (0, 255, 255), "magenta": (255, 0, 255), "gray": (128, 128, 128),
    "grey": (128, 128, 128), "silver": (192, 192, 192),
    "maroon": (128, 0, 0), "olive": (128, 128, 0), "lime": (0, 255, 0),
    "aqua": (0, 255, 255), "teal": (0, 128, 128), "navy": (0, 0, 128),
    "fuchsia": (255, 0, 255), "purple": (128, 0, 128),
    "orange": (255, 165, 0), "pink": (255, 192, 203),
    "brown": (165, 42, 42), "gold": (255, 215, 0),
    "indigo": (75, 0, 130), "violet": (238, 130, 238),
    "khaki": (240, 230, 140), "coral": (255, 127, 80),
    "salmon": (250, 128, 114), "plum": (221, 160, 221),
    "tan": (210, 180, 140), "beige": (245, 245, 220),
    "ivory": (255, 255, 240), "snow": (255, 250, 250),
    "lavender": (230, 230, 250), "crimson": (220, 20, 60),
    "tomato": (255, 99, 71), "orchid": (218, 112, 214),
    "turquoise": (64, 224, 208), "skyblue": (135, 206, 235),
    "steelblue": (70, 130, 180), "royalblue": (65, 105, 225),
    "dodgerblue": (30, 144, 255), "slateblue": (106, 90, 205),
    "lightblue": (173, 216, 230), "lightcyan": (224, 255, 255),
    "lightgray": (211, 211, 211), "lightgrey": (211, 211, 211),
    "lightgreen": (144, 238, 144), "lightyellow": (255, 255, 224),
    "lightpink": (255, 182, 193), "lightsalmon": (255, 160, 122),
    "darkgray": (169, 169, 169), "darkgrey": (169, 169, 169),
    "darkred": (139, 0, 0), "darkgreen": (0, 100, 0),
    "darkblue": (0, 0, 139), "darkorange": (255, 140, 0),
    "darkviolet": (148, 0, 211), "darkcyan": (0, 139, 139),
    "darkmagenta": (139, 0, 139), "darkkhaki": (189, 183, 107),
    "dimgray": (105, 105, 105), "dimgrey": (105, 105, 105),
    "whitesmoke": (245, 245, 245), "gainsboro": (220, 220, 220),
    "honeydew": (240, 255, 240), "azure": (240, 255, 255),
    "aliceblue": (240, 248, 255), "ghostwhite": (248, 248, 255),
    "mintcream": (245, 255, 250), "seashell": (255, 245, 238),
    "oldlace": (253, 245, 230), "linen": (250, 240, 230),
    "cornsilk": (255, 248, 220), "wheat": (245, 222, 179),
    "goldenrod": (218, 165, 32), "chocolate": (210, 105, 30),
    "sienna": (160, 82, 45), "peru": (205, 133, 63),
    "firebrick": (178, 34, 34), "indianred": (205, 92, 92),
    "rosybrown": (188, 143, 143), "sandybrown": (244, 164, 96),
    "seagreen": (46, 139, 87), "forestgreen": (34, 139, 34),
    "limegreen": (50, 205, 50), "springgreen": (0, 255, 127),
    "mediumseagreen": (60, 179, 113), "olivedrab": (107, 142, 35),
    "yellowgreen": (154, 205, 50), "lawngreen": (124, 252, 0),
    "palegreen": (152, 251, 152), "darkseagreen": (143, 188, 143),
    "cadetblue": (95, 158, 160), "powderblue": (176, 224, 230),
    "deepskyblue": (0, 191, 255), "cornflowerblue": (100, 149, 237),
    "mediumblue": (0, 0, 205), "midnightblue": (25, 25, 112),
    "rebeccapurple": (102, 51, 153), "mediumpurple": (147, 112, 219),
    "blueviolet": (138, 43, 226), "mediumorchid": (186, 85, 211),
    "deeppink": (255, 20, 147), "hotpink": (255, 105, 180),
    "palevioletred": (219, 112, 147), "mistyrose": (255, 228, 225),
    "peachpuff": (255, 218, 185), "navajowhite": (255, 222, 173),
    "moccasin": (255, 228, 181), "bisque": (255, 228, 196),
    "blanchedalmond": (255, 235, 205), "papayawhip": (255, 239, 213),
    "lemonchiffon": (255, 250, 205), "lightgoldenrodyellow": (250, 250, 210),
    "palegoldenrod": (238, 232, 170), "floralwhite": (255, 250, 240),
    "slategray": (112, 128, 144), "slategrey": (112, 128, 144),
    "lightslategray": (119, 136, 153), "lightslategrey": (119, 136, 153),
    "darkslategray": (47, 79, 79), "darkslategrey": (47, 79, 79),
    "darkslateblue": (72, 61, 139), "mediumslateblue": (123, 104, 238),
    "lightsteelblue": (176, 196, 222), "lightseagreen": (32, 178, 170),
    "mediumturquoise": (72, 209, 204), "paleturquoise": (175, 238, 238),
    "mediumaquamarine": (102, 205, 170), "aquamarine": (127, 255, 212),
    "chartreuse": (127, 255, 0), "greenyellow": (173, 255, 47),
    "darkolivegreen": (85, 107, 47), "darkgoldenrod": (184, 134, 11),
    "orangered": (255, 69, 0), "lightcoral": (240, 128, 128),
    "darksalmon": (233, 150, 122), "burlywood": (222, 184, 135),
    "antiquewhite": (250, 235, 215), "thistle": (216, 191, 216),
}

_CACHE = {}


def to_rgb(color, default=(0, 0, 0)):
    """Parse a color produced by layout.safe_color into (r, g, b)."""
    if not color:
        return default
    hit = _CACHE.get(color)
    if hit is not None:
        return hit
    value = color.strip().casefold()
    rgb = default
    if value.startswith("#"):
        hexpart = value[1:]
        try:
            if len(hexpart) == 3:
                rgb = tuple(int(c * 2, 16) for c in hexpart)
            elif len(hexpart) == 6:
                rgb = tuple(int(hexpart[i:i + 2], 16) for i in (0, 2, 4))
        except ValueError:
            rgb = default
    elif value in NAMED:
        rgb = NAMED[value]
    if len(_CACHE) < 10000:
        _CACHE[color] = rgb
    return rgb
