use std::collections::HashMap;
use std::fs;
use image::{DynamicImage, ImageReader};
use std::path::Path;
use template_matching::{MatchTemplateMethod, TemplateMatcher, Image};

#[derive(Debug)]
pub struct ExtremeLocation {
    pub value: f32,
    pub location: (usize, usize),
}

pub fn find_multiple_extremes(
    result: &Image,
    min_threshold: f32,
    max_threshold: f32,
    min_distance: usize,  // Minimum distance between peaks to consider them distinct
) -> Vec<ExtremeLocation> {
    let width = result.width as usize;
    let height = result.height as usize;
    let mut extremes = Vec::new();
    
    // Create a copy of the result that we can modify
    let mut result_copy = result.data.to_vec();
    
    // Keep finding minimums until we exceed the threshold
    while let Some(min_idx) = result_copy.iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
        .map(|(idx, &val)| (idx, val))
    {
        let min_value = min_idx.1;
        
        // Stop if we exceed the minimum threshold
        if min_value > min_threshold {
            break;
        }
        
        let y = min_idx.0 / width;
        let x = min_idx.0 % width;
        
        // Add this extreme to our list
        extremes.push(ExtremeLocation {
            value: min_value,
            location: (x, y),
        });
        
        // Suppress this minimum and its surrounding area to find the next one
        // This creates a "dead zone" around each detected minimum
        for dy in -(min_distance as isize)..=min_distance as isize {
            for dx in -(min_distance as isize)..=min_distance as isize {
                let new_y = y as isize + dy;
                let new_x = x as isize + dx;
                
                if new_y >= 0 && new_y < height as isize && 
                   new_x >= 0 && new_x < width as isize {
                    let idx = (new_y as usize) * width + (new_x as usize);
                    result_copy[idx] = f32::INFINITY;  // Mark this area as "visited"
                }
            }
        }
    }
    
    // Sort extremes by value so the best matches come first
    extremes.sort_by(|a, b| a.value.partial_cmp(&b.value).unwrap());
    
    extremes
}

pub fn load_item_images() -> Result<HashMap<String, DynamicImage>, Box<dyn std::error::Error>> {
    let mut item_map = HashMap::new();
    
    // Read the item list file
    let items_list = fs::read_to_string("captcha-screenshots/item-list.txt")?;
    
    // Process each item
    for item_name in items_list.lines() {
        let image_path = format!("captcha-screenshots/items/{}.png", item_name);
        if Path::new(&image_path).exists() {
            match ImageReader::open(&image_path)?.decode() {
                Ok(image) => {
                    item_map.insert(item_name.to_string(), image);
                },
                Err(e) => {
                    eprintln!("Failed to load image for {}: {}", item_name, e);
                }
            }
        } else {
            eprintln!("Image file not found for: {}", item_name);
        }
    }
    
    Ok(item_map)
}

fn main() {
    let frame_img = image::load_from_memory(include_bytes!("../frame_image.png")).unwrap();
    let frame_gray = frame_img.to_luma32f();
    println!("Loaded frame image with dimensions: {}x{}", frame_gray.width(), frame_gray.height());
    
    let mut matcher = TemplateMatcher::new();

    match load_item_images() {
        Ok(item_map) => {
            println!("Successfully loaded {} item images", item_map.len());

            // Process Grizzly image specifically
            if let Some(grizzly_image) = item_map.get("Grizzly") {
                println!("Template dimensions: {}x{}", grizzly_image.width(), grizzly_image.height());
                
                // Convert the template image to luma32f format
                let template_gray = grizzly_image.to_luma32f();
                
                // Create template matching images from raw data
                let frame_data = frame_gray.as_raw().to_vec();
                let template_data = template_gray.as_raw().to_vec();
                
                let frame_image = Image::new(
                    frame_data,
                    frame_gray.width(),
                    frame_gray.height(),
                );
                
                let template_image = Image::new(
                    template_data,
                    template_gray.width(),
                    template_gray.height(),
                );
                
                // Perform template matching
                matcher.match_template(
                    frame_image,
                    template_image,
                    MatchTemplateMethod::SumOfSquaredDifferences,
                );
                
                let result = matcher.wait_for_result().unwrap();
                
                // Find multiple potential matches
                let extremes = find_multiple_extremes(
                    &result,
                    25.0,  // min_threshold - adjust based on your needs
                    f32::INFINITY,  // max_threshold
                    20,  // min_distance in pixels
                );
                
                println!("Found {} potential matches:", extremes.len());
                for (i, extreme) in extremes.iter().enumerate() {
                    println!(
                        "Match {}: Location {:?} with confidence: {}", 
                        i + 1,
                        extreme.location,
                        extreme.value
                    );
                }
            }
        },
        Err(e) => {
            eprintln!("Error loading item images: {}", e);
        }
    }
}

