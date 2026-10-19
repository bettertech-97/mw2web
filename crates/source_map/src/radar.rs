//! The map's overhead picture for the IW4 minimap: `<zone>.radar.txt`
//! beside the map, which `cs2_textures --radar` writes from Counter-Strike's
//! own radar, naming the image (in `<zone>_radar.iwd`) and the world points
//! under its top-left and bottom-right corners. The picture has +Y up, which
//! is north on a Source map (see `northyaw` in the entity string).

use std::fmt::Write as _;

/// Counter-Strike overviews are laid out for a 1024-pixel picture whatever
/// the image's own size: `scale` is world units per pixel at that size.
const OVERVIEW_PIXELS: f32 = 1024.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Radar {
    pub image: String,
    pub upper_left: [f32; 2],
    pub lower_right: [f32; 2],
}

impl Radar {
    /// From a Counter-Strike overview (`resource/overviews/<map>.txt`):
    /// `pos_x`/`pos_y` is the world point at the picture's top-left corner
    /// and `scale` the units per pixel. Its `rotate` key does not change
    /// that: cs_italy says `rotate 1` and its spawns still fall on its
    /// CTSpawn/TSpawn marks this way.
    pub fn from_overview(image: &str, overview: &str) -> Result<Self, String> {
        let blocks = crate::keyvalues::parse(overview)?;
        let block = blocks.first().ok_or("overview: empty")?;
        let number = |key: &str| -> Result<f32, String> {
            block
                .get(key)
                .ok_or_else(|| format!("overview: no {key}"))?
                .trim()
                .parse::<f32>()
                .map_err(|error| format!("overview: {key}: {error}"))
        };
        let (x, y, scale) = (number("pos_x")?, number("pos_y")?, number("scale")?);
        if scale <= 0.0 {
            return Err(format!("overview: scale {scale}"));
        }
        let span = OVERVIEW_PIXELS * scale;
        Ok(Self {
            image: image.to_owned(),
            upper_left: [x, y],
            lower_right: [x + span, y - span],
        })
    }

    pub fn to_text(&self) -> String {
        format!(
            "# image\tupper_left_x\tupper_left_y\tlower_right_x\tlower_right_y\n{}\t{}\t{}\t{}\t{}\n",
            self.image, self.upper_left[0], self.upper_left[1], self.lower_right[0], self.lower_right[1]
        )
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let line = text
            .lines()
            .find(|line| !line.starts_with('#') && !line.trim().is_empty())
            .ok_or("radar: no row")?;
        let cells: Vec<&str> = line.split('\t').map(str::trim).collect();
        let [image, values @ ..] = cells.as_slice() else {
            return Err("radar: empty row".into());
        };
        let values = values
            .iter()
            .map(|cell| cell.parse::<f32>().map_err(|error| format!("radar: {cell}: {error}")))
            .collect::<Result<Vec<_>, _>>()?;
        let [ulx, uly, lrx, lry] = values.as_slice() else {
            return Err(format!("radar: {} numbers, want 4", values.len()));
        };
        if image.is_empty() || lrx <= ulx || lry >= uly {
            return Err(format!("radar: no picture in {line:?}"));
        }
        Ok(Self {
            image: (*image).to_owned(),
            upper_left: [*ulx, *uly],
            lower_right: [*lrx, *lry],
        })
    }

    /// The two `minimap_corner` script origins IW4's `setupMiniMap` reads.
    pub fn corner_entities(&self) -> String {
        let mut out = String::new();
        for [x, y] in [self.upper_left, self.lower_right] {
            let _ = writeln!(
                out,
                "{{\n\"classname\" \"script_origin\"\n\"targetname\" \"minimap_corner\"\n\"origin\" \"{x} {y} 0\"\n}}"
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::Radar;

    const MIRAGE: &str = r#""de_mirage"
{
	"material"	"overviews/de_mirage"	// texture file
	"pos_x"		"-3230"	// X coordinate,
	"pos_y"		"1713"	// Y coordinate,
	"scale"		"5.00" 	// and used scale used when taking the screenshot
	"rotate"	"0"	// map was rotated by 90 degress in image editor
}"#;

    #[test]
    fn overview_to_corners_and_back() {
        let radar = Radar::from_overview("compass_map_mp_mirage", MIRAGE).expect("overview");
        assert_eq!(radar.upper_left, [-3230.0, 1713.0]);
        assert_eq!(radar.lower_right, [1890.0, -3407.0]);
        assert_eq!(Radar::parse(&radar.to_text()), Ok(radar.clone()));
        let corners = radar.corner_entities();
        assert_eq!(corners.matches("\"minimap_corner\"").count(), 2);
        assert!(corners.contains("\"origin\" \"1890 -3407 0\""));
    }

    #[test]
    fn empty_pictures_are_refused() {
        assert!(Radar::from_overview("x", &MIRAGE.replace("\"5.00\"", "\"0\"")).is_err());
        assert!(Radar::parse("x\t0\t0\t0\t0\n").is_err());
    }
}
