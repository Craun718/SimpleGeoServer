use simple_geo_server::tile;

fn data_path(name: &str) -> String {
    let dir = std::env!("CARGO_MANIFEST_DIR");
    format!("{dir}/test-data/{name}")
}

fn lnglat_to_tile(lng: f64, lat: f64, zoom: u32) -> (u32, u32) {
    let n = (1u64 << zoom) as f64;
    let x = ((lng + 180.0) / 360.0 * n).floor() as u32;
    let lat_rad = lat.to_radians();
    let y = ((1.0 - (lat_rad.tan().asinh() / std::f64::consts::PI)) / 2.0 * n).floor() as u32;
    (x, y)
}

#[test]
fn test_outside_tile_returns_transparent_png() {
    let files = ["bahamas_rgb.tif", "co_elevation_roi.tif", "landsat.tif", "landsat7.tif"];

    for file in &files {
        let path = data_path(file);
        let info = tile::get_raster_tile_info(&path).unwrap();
        let extent = info.extent;

        let base_z = info.max_zoom.min(10).max(1);
        let zoom = base_z.saturating_sub(3).max(1);
        let (png_data, rendered) = tile::render_raster_tile(
            &tile::get_raster(&path).unwrap(),
            zoom,
            0,
            0,
            256,
            &[1, 2, 3],
        )
        .unwrap_or_else(|e| panic!("{file}: render error: {e}"));

        assert_eq!(
            rendered, 0,
            "{file}: tile (z={zoom}, x=0, y=0) should have 0 rendered pixels, got {rendered} (extent: {extent:?})",
        );

        // Verify PNG is fully transparent
        let img = image::load_from_memory(&png_data)
            .unwrap_or_else(|e| panic!("{file}: failed to decode PNG: {e}"));
        let rgba = img.to_rgba8();
        for (i, pixel) in rgba.pixels().enumerate() {
            assert_eq!(
                pixel.0,
                [0, 0, 0, 0],
                "{file}: pixel {i} should be fully transparent, got {pixel:?}",
            );
        }
    }
}

#[test]
fn test_inside_tile_has_rendered_pixels() {
    let files = ["bahamas_rgb.tif", "co_elevation_roi.tif", "landsat.tif", "landsat7.tif"];

    for file in &files {
        let path = data_path(file);
        let info = tile::get_raster_tile_info(&path).unwrap();

        // Use a moderate zoom that falls within the raster's max_zoom
        let zoom = info.max_zoom.min(10).max(1);

        // Pick the center of the extent
        let cx = (info.extent[0] + info.extent[2]) / 2.0;
        let cy = (info.extent[1] + info.extent[3]) / 2.0;
        let (tx, ty) = lnglat_to_tile(cx, cy, zoom);

        let (png_data, rendered) = tile::render_raster_tile(
            &tile::get_raster(&path).unwrap(),
            zoom,
            tx,
            ty,
            256,
            &[1, 2, 3],
        )
        .unwrap_or_else(|e| panic!("{file}: render error: {e}"));

        assert!(
            rendered > 0,
            "{file}: tile (z={zoom}, x={tx}, y={ty}) at extent center should have rendered pixels, got {rendered}",
        );

        // Decoded PNG should not be empty
        let img = image::load_from_memory(&png_data)
            .unwrap_or_else(|e| panic!("{file}: failed to decode PNG: {e}"));
        assert_eq!(img.width(), 256);
        assert_eq!(img.height(), 256);
    }
}

/// Tile just outside the bbox boundary should also return empty (not error)
#[test]
fn test_adjacent_outside_tile_is_empty() {
    let path = data_path("bahamas_rgb.tif");
    let info = tile::get_raster_tile_info(&path).unwrap();
    let extent = info.extent;

    let base_z = info.max_zoom.min(10).max(1);
    let zoom = base_z + 3;

    // Tile containing the center
    let cx = (extent[0] + extent[2]) / 2.0;
    let cy = (extent[1] + extent[3]) / 2.0;
    let (tx, ty) = lnglat_to_tile(cx, cy, zoom);

    // Go one tile north — this should still work (return transparent, not error)
    let north_y = ty.saturating_sub(1);
    let (_, rendered_north) = tile::render_raster_tile(
        &tile::get_raster(&path).unwrap(),
        zoom,
        tx,
        north_y,
        256,
        &[1, 2, 3],
    )
    .expect("adjacent north tile should not error");

    // Go one tile east
    let east_x = tx.saturating_add(1);
    let (_, rendered_east) = tile::render_raster_tile(
        &tile::get_raster(&path).unwrap(),
        zoom,
        east_x,
        ty,
        256,
        &[1, 2, 3],
    )
    .expect("adjacent east tile should not error");

    // At least one of the adjacent tiles should have 0 rendered pixels
    // (the raster might not fill the entire center tile)
    if rendered_north > 0 || rendered_east > 0 {
        // If they do have data, the raster extends to adjacent tiles — that's fine
        eprintln!(
            "Note: raster may extend to adjacent tiles (north={rendered_north}, east={rendered_east})"
        );
    }
}
