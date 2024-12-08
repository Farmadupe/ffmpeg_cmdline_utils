use image::{
    imageops::{resize, FilterType::Lanczos3},
    GenericImageView, GrayImage, RgbImage,
};

#[derive(Debug, Clone)]
pub struct VideoFrames {
    frames: Vec<RgbImage>,
}

impl VideoFrames {
    pub fn from_images(images: &[RgbImage]) -> Self {
        Self {
            frames: images.to_vec(),
        }
    }

    pub fn without_letterbox(&self) -> Self {
        type RgbView<'a> = image::SubImage<&'a RgbImage>;
        enum LetterboxColour {
            BlackWhite(u32),
            _AnyColour(u32),
        }
        use LetterboxColour::*;
        let cfg: LetterboxColour = BlackWhite(16);

        enum Side {
            Left,
            Right,
            Top,
            Bottom,
        }
        use Side::*;

        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
        struct Crop {
            orig_res: (u32, u32),
            left: u32,
            right: u32,
            top: u32,
            bottom: u32,
        }

        impl Crop {
            pub fn new(orig_res: (u32, u32), left: u32, right: u32, top: u32, bottom: u32) -> Self {
                //validate that the crop actually leaves some pixels. For now don't return an Err, just
                //replace with a crop of 0.
                Self {
                    orig_res,
                    left,
                    right,
                    top,
                    bottom,
                }
                .validate_or_default()
            }

            pub fn union(&self, other: &Self) -> Self {
                use std::cmp::min;

                Crop::new(
                    self.orig_res,
                    min(self.left, other.left),
                    min(self.right, other.right),
                    min(self.top, other.top),
                    min(self.bottom, other.bottom),
                )
            }

            pub fn as_view_args(&self) -> (u32, u32, u32, u32) {
                let (orig_width, orig_height) = self.orig_res;
                let coord_x = self.left;
                let coord_y = self.top;
                let width = orig_width - (self.left + self.right);
                let height = orig_height - (self.top + self.bottom);

                (coord_x, coord_y, width, height)
            }

            fn validate_or_default(self) -> Self {
                let (width, height) = self.orig_res;
                let valid = self.left + self.right < width && self.top + self.bottom < height;
                if valid {
                    self
                } else {
                    Crop::new(self.orig_res, 0, 0, 0, 0)
                }
            }
        }

        fn measure_frame(frame: &RgbView, colour: &LetterboxColour) -> Crop {
            let (width, height) = frame.dimensions();
            let measure_side = |side: Side| -> u32 {
                //get the window of pixels representing the next row/column to be checked
                let pixel_window = |idx: u32| -> RgbView {
                    #[rustfmt::skip]
                    let ret = match side {
                        //                   x                y                 width  height
                        Left   => frame.view(idx,             0,                1,     height),
                        Right  => frame.view(width - idx - 1, 0,                1,     height),
                        Top    => frame.view(0,               idx,              width, 1),
                        Bottom => frame.view(0,               height - idx - 1, width, 1),
                    };
                    ret
                };

                let is_letterbox = |strip: &RgbView| -> bool {
                    match colour {
                        BlackWhite(tol) => {
                            strip.pixels().all(|(_x, _y, image::Rgb::<u8>([r, g, b]))| {
                                let black_enough = r as u32 + g as u32 + b as u32 <= tol * 3;
                                let white_enough =
                                    r as u32 + g as u32 + b as u32 >= (u8::MAX as u32 - tol) * 3;
                                black_enough || white_enough
                            })
                        }
                        _AnyColour(tol) => {
                            //calculate range
                            let (mut min_r, mut min_g, mut min_b) = (u8::MAX, u8::MAX, u8::MAX);
                            let (mut max_r, mut max_g, mut max_b) = (u8::MIN, u8::MIN, u8::MIN);
                            for (_x, _y, image::Rgb::<u8>([ref r, ref g, ref b])) in strip.pixels()
                            {
                                #[rustfmt::skip]
                                {
                                    if r < &min_r {min_r = *r}
                                    if r > &max_r {max_r = *r}
                                    if g < &min_g {min_g = *g}
                                    if g > &max_g {max_g = *g}
                                    if b < &min_b {min_b = *b}
                                    if b > &max_b {max_b = *b}
                                };
                            }
                            let (range_r, range_g, range_b) = (
                                max_r.saturating_sub(min_r) as u32,
                                max_g.saturating_sub(min_g) as u32,
                                max_b.saturating_sub(min_b) as u32,
                            );

                            range_r + range_g + range_b <= tol * 3
                        }
                    }
                };

                let pix_range = match side {
                    Left | Right => 0..width,
                    Top | Bottom => 0..height,
                };

                pix_range.map(pixel_window).take_while(is_letterbox).count() as u32
            };

            Crop::new(
                (width, height),
                measure_side(Left),
                measure_side(Right),
                measure_side(Top),
                measure_side(Bottom),
            )
        }

        let crop = self
            .frames
            .iter()
            .map(|frame| measure_frame(&frame.view(0, 0, frame.width(), frame.height()), &cfg))
            .reduce(|x, y| x.union(&y))
            .unwrap();

        let (x, y, width, height) = crop.as_view_args();

        let cropped_frames = self
            .frames
            .iter()
            .map(|frame| frame.view(x, y, width, height).to_image())
            .collect();

        Self {
            frames: cropped_frames,
        }
    }

    pub fn resize(&self, width: u32, height: u32) -> Self {
        let resized_frames = self
            .frames
            .iter()
            .map(|frame| resize(frame, width, height, Lanczos3))
            .collect();

        Self {
            frames: resized_frames,
        }
    }

    pub fn into_inner(self) -> Vec<RgbImage> {
        self.frames
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn png_size(&self) -> u32 {
        //force resize to ensure that different resolutions are normalized
        let width = 1024;
        let height = 1024;

        //encode each frame as png and get its size
        self.frames
            .iter()
            .map(|frame| {
                let resized = resize(frame, width, height, Lanczos3);

                let mut buf = std::io::Cursor::new(vec![]);

                resized
                    .write_to(&mut buf, image::ImageFormat::Png)
                    .map(|()| buf.into_inner().len() as u32)
                    .unwrap_or_default()
            })
            .sum()
    }
}

pub struct GrayFramifiedVideo {
    frames: Vec<GrayImage>,
}

impl From<VideoFrames> for GrayFramifiedVideo {
    fn from(rgb: VideoFrames) -> Self {
        let images_gray = rgb
            .frames
            .into_iter()
            .map(|img| {
                let grey_buf: GrayImage = image::buffer::ConvertBuffer::convert(&img);
                grey_buf
            })
            .collect::<Vec<_>>();

        Self {
            frames: images_gray,
        }
    }
}

impl GrayFramifiedVideo {
    pub fn into_inner(self) -> Vec<GrayImage> {
        self.frames
    }
}
